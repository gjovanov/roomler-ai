// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P8-2a-7 — a worker's memory, bounded (Linux).
//!
//! Where the service delegates a cgroup with the memory controller to the
//! daemon (systemd's `Delegate=`), each worker runs in a cgroup of its own
//! below the unit's, its memory bounded and kept off swap: a worker that runs
//! away is ended by the kernel inside its own cgroup, and costs only its own
//! work (AC28). Where it delegates none, the worker runs without a bound, as
//! the FR-90 spec's P8-0 point 4 designed it.
//!
//! | Piece | As built |
//! |---|---|
//! | where | only where the daemon's own unit, as `run` identified it (`supervision::current`), answers `Delegate=yes` with `memory` among its `DelegateControllers`, asked of systemd within 5 s. As root a cgroup write succeeds without any delegation, so nothing is tried on a guess. Its silence is no answer: no worker runs until it has said, since it supervises this unit and a silence read as no would leave every worker unbounded (#3349) |
//! | the daemon | moved, with every process in the unit's cgroup, into a leaf of its own (`roomlerd`) as the first worker is started, since cgroup v2 gives a controller to child cgroups only of a cgroup with no processes of its own; then the unit's cgroup hands `memory` down |
//! | a worker | `hive-worker-<n>`, with `memory.max` at [`MAX_MEMORY`] (P8-2d-1: the device's worker, at its own bound, decision 72) and `memory.swap.max` at 0, since its memory holds the leases' values; it joins between fork and exec, writing itself into the cgroup's `cgroup.procs`, so it never runs a step outside it |
//! | its end | its cgroup's `memory.events` read for an OOM kill, which is logged as the reason, and the cgroup removed |
//! | a failure | where the unit delegates and the cgroups cannot be made, no worker runs: the bound is a limit as `RLIMIT_CORE` is (P8-2a-3), and a worker that cannot have it does not start. One that will pass is tried again by a later start (#3349, below); one that will not stays the answer until the daemon restarts |
//!
//! ⚠️ `supervision::detect_here` reads `/proc/self/cgroup` once, at the
//! daemon's start, before any worker exists: after the move the path's last
//! component is the leaf, which it would not take for the unit.
//!
//! ⚠️ **`cgroup.procs` is not what the kernel checks (#3349).** A process in
//! its exit cannot be moved: the write succeeds, and it stays listed until its
//! exit ends. A leader that exits before its threads stays listed longer than
//! it stays here: the zombie is listed while any thread of its process lives,
//! wherever that thread is. Its threads move, and once they have, the kernel
//! hands the controller down with the zombie still listed, so an empty list is
//! the wrong thing to wait for: boot's short-lived children cost one boot in
//! six that way. The hand-down's own `EBUSY` says whether a task is still here,
//! and that is what is waited out, for [`LEAVE_WITHIN`].

use std::fs::OpenOptions;
use std::future::Future;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{RwLock, RwLockReadGuard};
use std::time::{Duration, Instant};

use tracing::{info, warn};

use super::Running;

/// The most memory one session's worker holds, where its cgroup bounds it.
pub(crate) const MAX_MEMORY: u64 = 512 << 20;
/// The leaf the daemon's own processes move into.
const DAEMON_LEAF: &str = "roomlerd";
/// A worker's cgroup is this and a number, below the unit's.
const WORKER_PREFIX: &str = "hive-worker-";
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// How long systemd has to say whether it delegates.
const ASK_WITHIN: Duration = Duration::from_secs(5);
/// How long a task still in the unit's cgroup has to leave it. A boot waited
/// 3 to 31 ms in 40 measured, and a 600 MiB process ends in about 40 ms (the
/// FR-90 spec, §3i "#3349, as built"), so this is a slow host's margin. It
/// holds up that one worker and never the daemon's boot, and whoever waits
/// for the device's worker waits 10 s.
const LEAVE_WITHIN: Duration = Duration::from_secs(2);
/// How often that is looked at.
const LOOK_EVERY: Duration = Duration::from_millis(10);
/// The least time between two tries of a failure that passes, whoever asks:
/// sessions that start together share one try.
const TRY_AGAIN_AFTER: Duration = Duration::from_secs(5);

/// What the workers of this daemon run in.
enum Ready {
    /// No cgroup delegated: no bound.
    Unbounded,
    /// The unit's cgroup, made ready for its workers'.
    Bound(PathBuf),
    /// Delegated, and the cgroups could not be made, which will not pass by
    /// itself: no worker runs, and nothing is tried again.
    Broken(String),
    /// Not made yet, for a reason that passes: no worker runs until a later
    /// start's try makes them. `base` is what systemd said, none where it
    /// has said nothing, so that a try never turns on its asking again.
    Passing {
        base: Option<PathBuf>,
        why: String,
        tried: Instant,
    },
}

/// Why a try did not leave the workers' cgroups ready.
#[derive(Debug, PartialEq, Eq)]
enum NotReady {
    /// A later try may find it gone.
    Passing(String),
    /// It stays until the host, or the unit, is changed.
    Broken(String),
}

/// What this daemon's workers run in, and the one try at a time that decides
/// it.
struct Workers {
    /// `None` before the first try: only a spawn nothing prepared, a test's.
    ready: RwLock<Option<Ready>>,
    one_try: tokio::sync::Mutex<()>,
}

static READY: Workers = Workers::new();
static NEXT: AtomicU64 = AtomicU64::new(1);

impl Workers {
    const fn new() -> Self {
        Self {
            ready: RwLock::new(None),
            one_try: tokio::sync::Mutex::const_new(()),
        }
    }

    fn read(&self) -> RwLockReadGuard<'_, Option<Ready>> {
        self.ready.read().unwrap_or_else(|e| e.into_inner())
    }

    fn set(&self, ready: Ready) {
        *self.ready.write().unwrap_or_else(|e| e.into_inner()) = Some(ready);
    }

    /// Settle what the workers run in, by `ask`ing of systemd until it has
    /// said and `tries` at the cgroup it names. Once it is settled this does
    /// nothing, and a failure that passes is tried again here, at most once
    /// in `again_after`.
    async fn prepare<Fut>(
        &self,
        ask: impl Future<Output = Result<Option<PathBuf>, NotReady>>,
        tries: impl Fn(PathBuf) -> Fut,
        again_after: Duration,
    ) where
        Fut: Future<Output = Result<PathBuf, NotReady>>,
    {
        let _one = self.one_try.lock().await;
        let said = match &*self.read() {
            None => None,
            Some(Ready::Passing { base, tried, .. }) if tried.elapsed() >= again_after => {
                base.clone()
            }
            Some(_) => return,
        };
        let base = match said {
            Some(base) => Ok(Some(base)),
            None => ask.await,
        };
        let from = Instant::now();
        let (made, base) = match base {
            Ok(Some(base)) => (tries(base.clone()).await.map(Some), Some(base)),
            Ok(None) => (Ok(None), None),
            Err(no) => (Err(no), None),
        };
        self.set(match made {
            Ok(Some(base)) => {
                info!(
                    base = %base.display(),
                    waited_ms = from.elapsed().as_millis() as u64,
                    "hive: a worker runs in a cgroup of its own, its memory bounded"
                );
                Ready::Bound(base)
            }
            Ok(None) => {
                info!("hive: a worker runs without a memory bound: the service delegates no cgroup with the memory controller (systemd's Delegate=)");
                Ready::Unbounded
            }
            Err(NotReady::Passing(why)) => {
                warn!(%why, "hive: the workers' cgroups could not be made yet: no worker runs, and a later start tries again");
                Ready::Passing {
                    base,
                    why,
                    tried: Instant::now(),
                }
            }
            Err(NotReady::Broken(why)) => {
                warn!(%why, "hive: the service delegates a cgroup, and the workers' could not be made: no worker runs");
                Ready::Broken(why)
            }
        });
    }

    /// The cgroup the next worker runs in, holding it to `max`: `None` where
    /// it runs without a bound, and an error where it must have one and
    /// cannot, for now or for good: never `None` for a bound not made.
    fn slot(&self, max: u64) -> std::io::Result<Option<Slot>> {
        match &*self.read() {
            None | Some(Ready::Unbounded) => Ok(None),
            Some(Ready::Bound(base)) => slot_in(base, max).map(Some),
            Some(Ready::Broken(why) | Ready::Passing { why, .. }) => Err(std::io::Error::other(
                format!("its memory bound could not be made: {why}"),
            )),
        }
    }
}

/// Make the workers' cgroups ready (a worker's spawner asks, before each
/// start). Once settled it stays settled for the daemon's life.
pub(crate) async fn prepare() {
    READY.prepare(ask(), ready_at, TRY_AGAIN_AFTER).await;
}

/// Where this daemon's unit delegates the memory controller: its cgroup.
async fn ask() -> Result<Option<PathBuf>, NotReady> {
    let Some(crate::supervision::Supervision::Systemd(unit)) = crate::supervision::current() else {
        return Ok(None);
    };
    let mut cmd = tokio::process::Command::new("systemctl");
    if unit.user {
        cmd.arg("--user");
    }
    cmd.arg("show")
        .arg(unit.name)
        .arg("--property=Delegate")
        .arg("--property=DelegateControllers")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let show = said(cmd, ASK_WITHIN).await;
    let own = std::fs::read_to_string("/proc/self/cgroup").ok();
    base_of(show.as_deref().map_err(String::as_str), own.as_deref())
}

/// What `show`, a `systemctl show`, says within `within`, or in words why it
/// said nothing. This runs on a unit systemd is known to supervise, so none of
/// the three is its saying that the unit delegates nothing.
async fn said(mut show: tokio::process::Command, within: Duration) -> Result<String, String> {
    match tokio::time::timeout(within, show.output()).await {
        Ok(Ok(out)) if out.status.success() => {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        }
        Ok(Ok(out)) => Err(format!("systemctl failed to answer: {}", out.status)),
        Ok(Err(e)) => Err(format!("systemctl could not be run: {e}")),
        Err(_) => Err(format!("systemd did not answer within {within:?}")),
    }
}

/// The unit's cgroup to bound the workers in, from what systemd said of the
/// unit (`show`, or why it said nothing) and the daemon's own
/// `/proc/self/cgroup`: none unless the unit delegates the memory controller.
/// Where systemd said nothing, nothing is known, which is neither answer and
/// passes; where it delegates and there is no cgroup v2 hierarchy to bound
/// them in, an error.
fn base_of(
    show: Result<&str, &str>,
    own_cgroup: Option<&str>,
) -> Result<Option<PathBuf>, NotReady> {
    let show = show.map_err(|why| NotReady::Passing(why.to_string()))?;
    if !delegates_memory(show) {
        return Ok(None);
    }
    let path = own_cgroup
        .and_then(v2_path)
        .ok_or_else(|| NotReady::Broken("a delegated cgroup, and no cgroup v2 hierarchy".into()))?;
    Ok(Some(unit_dir(path)))
}

/// Whether a unit's `systemctl show` says it delegates the memory
/// controller.
fn delegates_memory(show: &str) -> bool {
    let prop = |key: &str| {
        show.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(str::trim)
    };
    prop("Delegate") == Some("yes")
        && prop("DelegateControllers").is_some_and(|c| c.split_whitespace().any(|c| c == "memory"))
}

/// The cgroup v2 path of `/proc/self/cgroup` (its `0::` line).
fn v2_path(content: &str) -> Option<&str> {
    content
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(str::trim)
}

/// The unit's own cgroup directory for the cgroup path the daemon is in:
/// above the leaf, where the daemon was moved into it already.
fn unit_dir(path: &str) -> PathBuf {
    let dir = Path::new(CGROUP_ROOT).join(path.trim_start_matches('/'));
    match dir.file_name() {
        Some(leaf) if leaf == DAEMON_LEAF => dir.parent().map(Path::to_path_buf).unwrap_or(dir),
        _ => dir,
    }
}

/// Make the unit's cgroup `base` ready for its workers': every process in
/// it moved into the daemon's leaf, the memory controller handed down, and
/// any cgroup a worker of an earlier daemon left removed.
async fn ready_at(base: PathBuf) -> Result<PathBuf, NotReady> {
    ready_within(base, LEAVE_WITHIN, hand_down).await
}

/// The unit's cgroup hands the memory controller to the ones below it.
fn hand_down(base: &Path) -> std::io::Result<()> {
    std::fs::write(base.join("cgroup.subtree_control"), "+memory")
}

/// [`ready_at`], what is still here given `within` to leave. Everything but
/// that wait is a failure that will not pass.
async fn ready_within(
    base: PathBuf,
    within: Duration,
    hand_down: impl Fn(&Path) -> std::io::Result<()>,
) -> Result<PathBuf, NotReady> {
    let controllers = std::fs::read_to_string(base.join("cgroup.controllers"))
        .map_err(|e| NotReady::Broken(format!("{}: {e}", base.display())))?;
    if !controllers.split_whitespace().any(|c| c == "memory") {
        return Err(NotReady::Broken(format!(
            "{}: no memory controller here",
            base.display()
        )));
    }
    let leaf = base.join(DAEMON_LEAF);
    match std::fs::create_dir(&leaf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(NotReady::Broken(format!("{}: {e}", leaf.display()))),
    }
    // New children can be forked into the unit's cgroup as its processes
    // move, and a process in its exit stays: until the kernel says none of
    // it is left here.
    let from = Instant::now();
    loop {
        let procs = std::fs::read_to_string(base.join("cgroup.procs"))
            .map_err(|e| NotReady::Broken(format!("{}: {e}", base.display())))?;
        for pid in procs.split_whitespace() {
            match std::fs::write(leaf.join("cgroup.procs"), pid) {
                Ok(()) => {}
                // Gone meanwhile.
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
                Err(e) => {
                    return Err(NotReady::Broken(format!(
                        "moving {pid} into {}: {e}",
                        leaf.display()
                    )));
                }
            }
        }
        match hand_down(&base) {
            Ok(()) => break,
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {}
            Err(e) => {
                return Err(NotReady::Broken(format!(
                    "{}: handing memory down: {e}",
                    base.display()
                )));
            }
        }
        if from.elapsed() >= within {
            return Err(NotReady::Passing(format!(
                "{}: still here after {within:?}: {}",
                base.display(),
                still_here(&base)
            )));
        }
        tokio::time::sleep(LOOK_EVERY).await;
    }
    if let Ok(entries) = std::fs::read_dir(&base) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(WORKER_PREFIX)
            {
                let _ = std::fs::remove_dir(entry.path());
            }
        }
    }
    Ok(base)
}

/// What `base` lists, for a person: each pid, with its name while it has one.
/// None listed is a thread of a process that has moved, still in its exit.
fn still_here(base: &Path) -> String {
    let procs = std::fs::read_to_string(base.join("cgroup.procs")).unwrap_or_default();
    let named: Vec<String> = procs
        .split_whitespace()
        .map(
            |pid| match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
                Ok(comm) => format!("{pid} ({})", comm.trim().escape_debug()),
                Err(_) => pid.to_string(),
            },
        )
        .collect();
    if named.is_empty() {
        "a thread in its exit".to_string()
    } else {
        named.join(", ")
    }
}

/// A worker's cgroup, made before its spawn.
pub(crate) struct Slot {
    dir: PathBuf,
    /// Its `cgroup.procs`, which the child writes itself into: the parent's
    /// copy goes once the child is in.
    procs: Option<OwnedFd>,
    /// Its `memory.max`.
    max: u64,
}

/// The cgroup the next worker runs in ([`Workers::slot`]).
pub(crate) fn slot(max: u64) -> std::io::Result<Option<Slot>> {
    READY.slot(max)
}

static NO_SWAP_ACCOUNT: AtomicBool = AtomicBool::new(false);

fn slot_in(base: &Path, max: u64) -> std::io::Result<Slot> {
    let dir = base.join(format!(
        "{WORKER_PREFIX}{}",
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&dir)?;
    let made = (|| {
        std::fs::write(dir.join("memory.max"), max.to_string())?;
        // Its memory holds the leases' values: never on swap.
        match std::fs::write(dir.join("memory.swap.max"), "0") {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !NO_SWAP_ACCOUNT.swap(true, Ordering::Relaxed) {
                    warn!(
                        "hive: this kernel keeps no swap account for a cgroup: a worker's memory can go to swap"
                    );
                }
            }
            Err(e) => return Err(e),
        }
        let procs = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(dir.join("cgroup.procs"))?;
        Ok(OwnedFd::from(procs))
    })();
    match made {
        Ok(procs) => Ok(Slot {
            dir,
            procs: Some(procs),
            max,
        }),
        Err(e) => {
            let _ = std::fs::remove_dir(&dir);
            Err(e)
        }
    }
}

impl Slot {
    /// The descriptor a child writes itself into, between fork and exec.
    pub(crate) fn procs_fd(&self) -> RawFd {
        self.procs.as_ref().map_or(-1, AsRawFd::as_raw_fd)
    }

    /// The child is spawned, and in: the parent's copy goes.
    pub(crate) fn spawned(&mut self) {
        self.procs = None;
    }

    /// At the worker's end: how many times the kernel killed in its cgroup
    /// for its memory; and the cgroup removed.
    fn end(&self) -> u64 {
        let kills = std::fs::read_to_string(self.dir.join("memory.events"))
            .map(|e| oom_kills(&e))
            .unwrap_or(0);
        let _ = std::fs::remove_dir(&self.dir);
        kills
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// `oom_kill` of a `memory.events`.
fn oom_kills(events: &str) -> u64 {
    events
        .lines()
        .find_map(|l| l.strip_prefix("oom_kill "))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0)
}

/// A worker in a cgroup of its own.
pub(crate) struct Bounded {
    pub(crate) child: tokio::process::Child,
    pub(crate) slot: Option<Slot>,
}

#[async_trait::async_trait]
impl Running for Bounded {
    fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    async fn kill(&mut self) {
        let pid = self.child.id();
        let _ = self.child.kill().await;
        if let Some(slot) = self.slot.take() {
            let kills = slot.end();
            if kills > 0 {
                warn!(
                    ?pid,
                    kills,
                    max_mib = slot.max >> 20,
                    "hive: a worker was ended by the kernel for its memory: past its bound"
                );
            }
        }
    }
}

/// A test's: the workers of this process bounded in `base`, made ready as a
/// daemon's would be.
#[cfg(test)]
pub(crate) async fn ready_for_test(base: &Path) -> Result<(), String> {
    let base = ready_at(base.to_path_buf())
        .await
        .map_err(|no| format!("{no:?}"))?;
    READY.set(Ready::Bound(base));
    Ok(())
}

#[cfg(test)]
mod tests;

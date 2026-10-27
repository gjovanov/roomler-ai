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
//! | where | only where the daemon's own unit, as `run` identified it (`supervision::current`), answers `Delegate=yes` with `memory` among its `DelegateControllers`, asked of systemd within 5 s. As root a cgroup write succeeds without any delegation, so nothing is tried on a guess |
//! | the daemon | moved, with every process in the unit's cgroup, into a leaf of its own (`roomlerd`) as the first worker is started, since cgroup v2 gives a controller to child cgroups only of a cgroup with no processes of its own; then the unit's cgroup hands `memory` down |
//! | a worker | `hive-worker-<n>`, with `memory.max` at [`MAX_MEMORY`] and `memory.swap.max` at 0, since its memory holds the leases' values; it joins between fork and exec, writing itself into the cgroup's `cgroup.procs`, so it never runs a step outside it |
//! | its end | its cgroup's `memory.events` read for an OOM kill, which is logged as the reason, and the cgroup removed |
//! | a failure | where the unit delegates and the cgroups cannot be made, no worker runs: the bound is a limit as `RLIMIT_CORE` is (P8-2a-3), and a worker that cannot have it does not start |
//!
//! ⚠️ `supervision::detect_here` reads `/proc/self/cgroup` once, at the
//! daemon's start, before any worker exists: after the move the path's last
//! component is the leaf, which it would not take for the unit.

use std::fs::OpenOptions;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::OnceCell;
use tracing::{info, warn};

use super::Running;

/// The most memory one worker holds, where its cgroup bounds it.
pub(crate) const MAX_MEMORY: u64 = 512 << 20;
/// The leaf the daemon's own processes move into.
const DAEMON_LEAF: &str = "roomlerd";
/// A worker's cgroup is this and a number, below the unit's.
const WORKER_PREFIX: &str = "hive-worker-";
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// How long systemd has to say whether it delegates.
const ASK_WITHIN: Duration = Duration::from_secs(5);

/// What the workers of this daemon run in.
enum Ready {
    /// No cgroup delegated: no bound.
    Unbounded,
    /// The unit's cgroup, made ready for its workers'.
    Bound(PathBuf),
    /// Delegated, and the cgroups could not be made: no worker runs.
    Broken(String),
}

static READY: OnceCell<Ready> = OnceCell::const_new();
static NEXT: AtomicU64 = AtomicU64::new(1);

/// Make the workers' cgroups ready, once for the daemon's life (the first
/// worker's spawner asks, before it starts one).
pub(crate) async fn prepare() {
    READY
        .get_or_init(|| async {
            match ready().await {
                Ok(Some(base)) => {
                    info!(
                        base = %base.display(), max_mib = MAX_MEMORY >> 20,
                        "hive: a session's worker runs in a cgroup of its own, its memory bounded"
                    );
                    Ready::Bound(base)
                }
                Ok(None) => {
                    info!("hive: a session's worker runs without a memory bound: the service delegates no cgroup with the memory controller (systemd's Delegate=)");
                    Ready::Unbounded
                }
                Err(why) => {
                    warn!(%why, "hive: the service delegates a cgroup, and the workers' could not be made: no worker runs");
                    Ready::Broken(why)
                }
            }
        })
        .await;
}

/// Where this daemon's unit delegates the memory controller: its cgroup,
/// made ready ([`ready_at`]).
async fn ready() -> Result<Option<PathBuf>, String> {
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
    let show = match tokio::time::timeout(ASK_WITHIN, cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => {
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        }
        _ => None,
    };
    let own = std::fs::read_to_string("/proc/self/cgroup").ok();
    match base_of(show.as_deref(), own.as_deref())? {
        Some(base) => ready_at(&base).map(Some),
        None => Ok(None),
    }
}

/// The unit's cgroup to bound the workers in, from what systemd said of the
/// unit (`show`, none where it did not answer) and the daemon's own
/// `/proc/self/cgroup`: none unless the unit delegates the memory
/// controller, since not known to delegate is not delegating; and an error
/// where it does and there is no cgroup v2 hierarchy to bound them in.
fn base_of(show: Option<&str>, own_cgroup: Option<&str>) -> Result<Option<PathBuf>, String> {
    if !show.is_some_and(delegates_memory) {
        return Ok(None);
    }
    let path = own_cgroup
        .and_then(v2_path)
        .ok_or("a delegated cgroup, and no cgroup v2 hierarchy")?;
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
fn ready_at(base: &Path) -> Result<PathBuf, String> {
    let controllers = std::fs::read_to_string(base.join("cgroup.controllers"))
        .map_err(|e| format!("{}: {e}", base.display()))?;
    if !controllers.split_whitespace().any(|c| c == "memory") {
        return Err(format!("{}: no memory controller here", base.display()));
    }
    let leaf = base.join(DAEMON_LEAF);
    match std::fs::create_dir(&leaf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("{}: {e}", leaf.display())),
    }
    // New children can be forked into the unit's cgroup as its processes
    // move: until it holds none.
    let mut emptied = false;
    for _ in 0..8 {
        let procs = std::fs::read_to_string(base.join("cgroup.procs"))
            .map_err(|e| format!("{}: {e}", base.display()))?;
        if procs.split_whitespace().next().is_none() {
            emptied = true;
            break;
        }
        for pid in procs.split_whitespace() {
            match std::fs::write(leaf.join("cgroup.procs"), pid) {
                Ok(()) => {}
                // Gone meanwhile.
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
                Err(e) => return Err(format!("moving {pid} into {}: {e}", leaf.display())),
            }
        }
    }
    if !emptied {
        return Err(format!("{}: it kept gaining processes", base.display()));
    }
    std::fs::write(base.join("cgroup.subtree_control"), "+memory")
        .map_err(|e| format!("{}: handing memory down: {e}", base.display()))?;
    if let Ok(entries) = std::fs::read_dir(base) {
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
    Ok(base.to_path_buf())
}

/// A worker's cgroup, made before its spawn.
pub(crate) struct Slot {
    dir: PathBuf,
    /// Its `cgroup.procs`, which the child writes itself into: the parent's
    /// copy goes once the child is in.
    procs: Option<OwnedFd>,
}

/// The cgroup the next worker runs in: `None` where it runs without a bound,
/// and an error where it must have one and cannot.
pub(crate) fn slot() -> std::io::Result<Option<Slot>> {
    match READY.get() {
        None | Some(Ready::Unbounded) => Ok(None),
        Some(Ready::Bound(base)) => slot_in(base).map(Some),
        Some(Ready::Broken(why)) => Err(std::io::Error::other(format!(
            "its memory bound could not be made: {why}"
        ))),
    }
}

static NO_SWAP_ACCOUNT: AtomicBool = AtomicBool::new(false);

fn slot_in(base: &Path) -> std::io::Result<Slot> {
    let dir = base.join(format!(
        "{WORKER_PREFIX}{}",
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&dir)?;
    let made = (|| {
        std::fs::write(dir.join("memory.max"), MAX_MEMORY.to_string())?;
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
                    max_mib = MAX_MEMORY >> 20,
                    "hive: a session's worker was ended by the kernel for its memory: past its bound"
                );
            }
        }
    }
}

/// A test's: the workers of this process bounded in `base`, made ready as a
/// daemon's would be.
#[cfg(test)]
pub(crate) fn ready_for_test(base: &Path) -> Result<(), String> {
    let base = ready_at(base)?;
    READY
        .set(Ready::Bound(base))
        .map_err(|_| "already made".to_string())
}

#[cfg(test)]
mod tests;

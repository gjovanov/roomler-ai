// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-151 — the gate of the Mac's root update helper (`com.roomler.update`):
//! an automatic update waits for live work on a Mac as it does on Windows and
//! Linux (#3429, [`crate::updater::periodic_gate`]).
//! docs/fr/FR-151-mac-update-waits-for-live-work.md
//!
//! The helper installs a package whose postinstall restarts both halves of the
//! Mac: the root daemon and the console person's daemon. Before it installs it
//! reads each half ([`Reading`]): the root half over the system socket, the
//! person's half through FR-147's child (`roomlerd one-row-home live-work`).
//! This module is the decision, pure and compiled on every OS; the effects are
//! the macOS arm of `updater::run_update_helper`.
//!
//! | Reading | When | Counts as |
//! |---|---|---|
//! | `answered` | the daemon answered `live_work` | its counts |
//! | `not_running` | no socket, a socket nobody serves, nobody at the console | nothing |
//! | `older` | the daemon does not know the verb | one unit of live work |
//! | `no_answer` | anything else: silence, an error, a malformed reply | one unit of live work |
//!
//! - **The budget** is #3429's: a defer rechecks after an hour (R5b: two
//!   minutes for the root half's network transition alone), and the cap
//!   forces the install, with a warning that names what it cuts. Root keeps
//!   it in [`BUDGET_FILE`] ([`Budget`]), so it survives a reboot, and only a
//!   due cycle counts ([`Budget::due`]): a wake never does.
//! - **Who skips the wait** ([`helper_arm`]): a fresh ask recorded by the root
//!   half (an operator's push) skips it over everything; a fresh ask recorded
//!   by the person's half (a push to that row, or "Apply update") skips it
//!   over that half's work and the root half's remote-control sessions only.
//! - **The wait** ([`next_step`]): the helper stays up, ticks every [`TICK`],
//!   reads again at `next_due` or at a wake, and exits on the opt-out, a
//!   replaced program, SIGTERM or [`AWAKE_CEILING`].
//! - **The kill switch** [`KILL_SWITCH`] installs with no reading, as before.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tunnel_core::localapi::LiveWorkInfo;

use crate::busy::Busy;
use crate::updater::{
    DeferDecision, MAX_CONSECUTIVE_DEFERS, TRANSFER_DEFER_RECHECK, UPDATE_HELPER_MIN_GAP,
};

/// How old an ask may be and still skip the wait: the helper it woke runs
/// within seconds, so an older one is a stale record, and the cycle is a wake.
pub const ASK_FRESH: Duration = Duration::from_secs(10 * 60);
/// The bound on each half's answer to `live_work`.
pub const ASK_TIMEOUT: Duration = Duration::from_secs(5);
/// How often a waiting helper looks at its markers, the wake file and the clock.
pub const TICK: Duration = Duration::from_secs(5);
/// A waiting helper's awake time at most (an `Instant`, which stops while the
/// Mac sleeps). A chain takes about seven hours, so this never ends a healthy
/// one; the budget stays for the next run.
pub const AWAKE_CEILING: Duration = Duration::from_secs(8 * 60 * 60);
/// The budget's file, in root's log folder beside `update-attempt`.
pub const BUDGET_FILE: &str = "update-defers.json";
/// The lock one helper holds while it runs, beside the budget.
pub const LOCK_FILE: &str = "update-helper.lock";
/// The most bytes the budget's file may hold.
pub const BUDGET_MAX_BYTES: usize = 4096;
/// Root's marker that gives back the install without a reading.
pub const KILL_SWITCH: &str = "/etc/roomler/install-over-live-work";
const BUDGET_FORMAT: u32 = 1;

/// One half's answer to `live_work`, as the gate counts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    Answered(LiveWorkInfo),
    /// No socket, a socket nobody serves, or nobody at the console: a stopped
    /// daemon serves nothing an install could cut.
    NotRunning,
    /// The daemon answered that it does not know the verb (older than FR-151).
    Older,
    /// No answer within the bound, an error, or a reply that did not parse.
    NoAnswer,
}

impl Reading {
    /// The word the log line uses.
    pub fn word(&self) -> &'static str {
        match self {
            Reading::Answered(_) => "answered",
            Reading::NotRunning => "not_running",
            Reading::Older => "older",
            Reading::NoAnswer => "no_answer",
        }
    }

    /// Unread: counted as one unit of live work, so a wrong "idle" never cuts
    /// a session. A wrong "busy" costs one defer, and defers are capped.
    pub fn unread(&self) -> bool {
        matches!(self, Reading::Older | Reading::NoAnswer)
    }

    /// The counts this half answered, in `Busy`'s words.
    fn counts(&self) -> Busy {
        match self {
            Reading::Answered(i) => Busy {
                rc_sessions: i.rc_sessions as usize,
                ssh_connections: i.ssh_connections as usize,
                exec_runs: i.exec_runs as usize,
                work: i.work as usize,
                net_transition: i.net_transition,
                install_in_flight: false,
            },
            _ => Busy::IDLE,
        }
    }

    /// This half recorded an ask no older than [`ASK_FRESH`].
    fn fresh_ask(&self) -> bool {
        matches!(self, Reading::Answered(i) if i.ask_age_secs.is_some_and(|age| age <= ASK_FRESH.as_secs()))
    }
}

/// The defer budget root keeps across runs: the defers in a row so far, and
/// when the next counted cycle is due (unix seconds on the wall clock, which
/// keeps running while the Mac sleeps). No chain is pending when `defers`
/// is 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Budget {
    pub defers: u32,
    pub next_due: u64,
}

/// [`Budget`] on disk: `{"v":1,"defers":3,"next_due":1795900000}`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BudgetFile {
    v: u32,
    defers: u32,
    next_due: u64,
}

impl Budget {
    /// No chain pending.
    pub const RESET: Budget = Budget {
        defers: 0,
        next_due: 0,
    };

    /// A chain of defers is pending.
    pub fn chain(&self) -> bool {
        self.defers > 0
    }

    /// The file's bytes, strictly: at most [`BUDGET_MAX_BYTES`], one object
    /// with nothing unknown in it, of this format, `defers` within the cap.
    /// `None` reads as no chain pending.
    pub fn parse(bytes: &[u8]) -> Option<Budget> {
        if bytes.len() > BUDGET_MAX_BYTES {
            return None;
        }
        let file: BudgetFile = serde_json::from_slice(bytes).ok()?;
        (file.v == BUDGET_FORMAT && file.defers <= MAX_CONSECUTIVE_DEFERS).then_some(Budget {
            defers: file.defers,
            next_due: file.next_due,
        })
    }

    /// The bytes [`Budget::parse`] reads back.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&BudgetFile {
            v: BUDGET_FORMAT,
            defers: self.defers,
            next_due: self.next_due,
        })
        .unwrap_or_default()
    }

    /// Whether a cycle at `now` counts against the budget: no chain is
    /// pending, `next_due` has come, or `next_due` is further away than one
    /// recheck (the clock went back). Inside a chain only root's own
    /// `next_due` counts, so a wake can neither hold an update off nor bring
    /// the force closer.
    pub fn due(&self, now: u64) -> bool {
        !self.chain()
            || now >= self.next_due
            || self.next_due - now > TRANSFER_DEFER_RECHECK.as_secs()
    }
}

/// What one cycle does with the readings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperArm {
    /// Install now. `warning` names what a forced install cuts; `forced` is a
    /// fresh ask or the cap, which skips the second reading.
    Install {
        warning: Option<String>,
        forced: bool,
    },
    /// Do not install now: keep `budget`, counted when the cycle was due.
    Defer { budget: Budget, counted: bool },
}

/// Whether the helper reads the halves before it installs: not with the kill
/// switch, which installs as before FR-151.
pub fn reads_before_install(kill_switch: bool) -> bool {
    !kill_switch
}

/// The arm of one cycle at `now`, from the two readings and the budget.
pub fn helper_arm(root: &Reading, person: &Reading, budget: Budget, now: u64) -> HelperArm {
    let forced = || HelperArm::Install {
        warning: forced_warning(root, person),
        forced: true,
    };
    // An operator's push: only root serves the system socket, so only root
    // can have recorded it, and it skips the wait over both halves.
    if root.fresh_ask() {
        return forced();
    }
    // The person's own ask (a push to their row, or "Apply update") skips it
    // over their own half's work and the root half's remote-control sessions,
    // which are on that person's desktop: never over the root half's SSH
    // connections, runs, transfers or recording, a network transition, or a
    // root half it could not read.
    let root_beyond_sessions = match root {
        Reading::Answered(i) => {
            i.ssh_connections > 0 || i.exec_runs > 0 || i.work > 0 || i.net_transition
        }
        Reading::NotRunning => false,
        Reading::Older | Reading::NoAnswer => true,
    };
    if person.fresh_ask() && !root_beyond_sessions {
        return forced();
    }
    let active = root.counts().cuttable_count()
        + person.counts().cuttable_count()
        + usize::from(root.unread())
        + usize::from(person.unread());
    // R5b reads the root half alone: it owns the overlay and the netstate.
    let transition = root.counts().net_transition;
    if !budget.due(now) {
        // A wake inside a chain: it installs only when nothing is live, and
        // it never counts.
        return if active == 0 && !transition {
            HelperArm::Install {
                warning: None,
                forced: false,
            }
        } else {
            HelperArm::Defer {
                budget,
                counted: false,
            }
        };
    }
    // #3429's rule, on what both halves serve: `periodic_gate` reads only
    // the cut count and the transition, so the unread halves join the count.
    let gate = Busy {
        rc_sessions: active,
        net_transition: transition,
        ..Busy::IDLE
    };
    match crate::updater::periodic_gate(&gate, budget.defers) {
        (DeferDecision::Proceed, _) => HelperArm::Install {
            warning: None,
            forced: false,
        },
        (DeferDecision::DeferOnce, recheck) => HelperArm::Defer {
            budget: Budget {
                defers: budget.defers + 1,
                next_due: now.saturating_add(recheck.as_secs()),
            },
            counted: true,
        },
        (DeferDecision::ForceAfterDefers, _) => forced(),
    }
}

/// The arm after the download, from a second reading: a forced arm stands
/// (it skips the second reading), anything else is decided again on the same
/// budget and the same `now`, so a defer drops the download.
pub fn second_arm(
    first: &HelperArm,
    root: &Reading,
    person: &Reading,
    budget: Budget,
    now: u64,
) -> HelperArm {
    match first {
        HelperArm::Install { forced: true, .. } => first.clone(),
        _ => helper_arm(root, person, budget, now),
    }
}

/// What an install now would cut or wedge, for the log: both halves' counts,
/// the root half's network transition, and each unread half.
pub fn what_it_cuts(root: &Reading, person: &Reading) -> String {
    let (r, p) = (root.counts(), person.counts());
    let busy = Busy {
        rc_sessions: r.rc_sessions + p.rc_sessions,
        ssh_connections: r.ssh_connections + p.ssh_connections,
        exec_runs: r.exec_runs + p.exec_runs,
        work: r.work + p.work,
        // R5b reads the root half alone: it owns the overlay and the netstate.
        net_transition: r.net_transition,
        install_in_flight: false,
    };
    let mut parts = Vec::new();
    if busy != Busy::IDLE {
        parts.push(busy.to_string());
    }
    for (half, reading) in [("the root half", root), ("the person's half", person)] {
        if reading.unread() {
            parts.push(format!("{half}, unread ({})", reading.word()));
        }
    }
    if parts.is_empty() {
        "nothing".to_string()
    } else {
        parts.join(", ")
    }
}

/// The warning a forced install gives: `None` when it cuts nothing.
pub fn forced_warning(root: &Reading, person: &Reading) -> Option<String> {
    let what = what_it_cuts(root, person);
    (what != "nothing").then(|| format!("forced update proceeding despite: {what}"))
}

/// The defer line's fields, after `update-helper: deferring the install:`.
pub fn defer_line(
    root: &Reading,
    person: &Reading,
    budget: Budget,
    now: u64,
    counted: bool,
) -> String {
    format!(
        "busy={} consecutive_defers={} next_check_secs={} cycle={} root={} person={}",
        what_it_cuts(root, person),
        budget.defers,
        budget.next_due.saturating_sub(now),
        if counted { "due" } else { "wake" },
        root.word(),
        person.word(),
    )
}

/// What the feed said, when the cycle installs nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeSaid {
    /// The Mac runs the newest release.
    UpToDate,
    /// The probe or the download failed.
    Failed,
}

/// The budget after a cycle that installed nothing: up to date resets it, a
/// failed probe or download keeps it.
pub fn budget_after_probe(said: ProbeSaid, budget: Budget) -> Budget {
    match said {
        ProbeSaid::UpToDate => Budget::RESET,
        ProbeSaid::Failed => budget,
    }
}

/// Keep `new` as the budget in memory, then write it. The budget in memory is
/// the one that counts within a run, so a failed write (a full disk) cannot
/// keep a chain from the force; the error is returned to be logged.
pub fn keep_budget(
    mem: &mut Budget,
    new: Budget,
    write: impl FnOnce(&Budget) -> std::io::Result<()>,
) -> std::io::Result<()> {
    *mem = new;
    write(mem)
}

/// What a waiting helper sees at one tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Observed {
    /// SIGTERM came (launchd's bootout, a shutdown).
    pub stop: bool,
    /// `/etc/roomler/disable-auto-update` or another opt-out marker is there.
    pub opt_out: bool,
    /// [`KILL_SWITCH`] is there.
    pub kill_switch: bool,
    /// The helper's own program changed device or inode (a manual install).
    pub program_replaced: bool,
    /// The helper's awake time since it started.
    pub awake: Duration,
    /// The wall clock, unix seconds.
    pub now: u64,
    /// The wake file came back since the last cycle.
    pub wake: bool,
    /// Awake time since the last cycle ended.
    pub since_cycle: Duration,
}

/// Why a waiting helper exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitWhy {
    Stopped,
    OptedOut,
    Replaced,
    Ceiling,
}

/// A waiting helper's next step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Exit without installing; the budget stays for the next run.
    Exit(ExitWhy),
    /// The kill switch appeared: install with no reading.
    InstallOverAll,
    /// Run a cycle: due when [`Budget::due`] says so, else a wake.
    Cycle,
    /// Nothing to do for this long; the helper looks again every [`TICK`].
    Wait(Duration),
}

/// A waiting helper's next step, in this order: SIGTERM, the opt-out, the kill
/// switch, a replaced program, the ceiling, a due cycle, a wake whose gap has
/// passed, a wake still inside its gap (wait out the rest), else one tick.
pub fn next_step(seen: &Observed, budget: Budget) -> Step {
    if seen.stop {
        return Step::Exit(ExitWhy::Stopped);
    }
    if seen.opt_out {
        return Step::Exit(ExitWhy::OptedOut);
    }
    if seen.kill_switch {
        return Step::InstallOverAll;
    }
    if seen.program_replaced {
        return Step::Exit(ExitWhy::Replaced);
    }
    if seen.awake >= AWAKE_CEILING {
        return Step::Exit(ExitWhy::Ceiling);
    }
    if budget.due(seen.now) {
        return Step::Cycle;
    }
    if seen.wake {
        return match UPDATE_HELPER_MIN_GAP.checked_sub(seen.since_cycle) {
            Some(rest) if !rest.is_zero() => Step::Wait(rest),
            _ => Step::Cycle,
        };
    }
    Step::Wait(TICK)
}

/// What one cycle's arm leaves as the budget: an install resets it, just
/// before `installer(8)` runs; a defer keeps the one it carries.
pub fn budget_for(arm: &HelperArm, budget: Budget) -> Budget {
    match arm {
        HelperArm::Install { .. } => Budget::RESET,
        HelperArm::Defer { budget: kept, .. } => {
            let _ = budget;
            *kept
        }
    }
}

/// #4423 — how long `roomlerd self-update`, run as a person, looks for the
/// root half ([`root_half_seen`]): the bound `roomler status` gives the same
/// look (`localapi::other_daemon_socket`), so a wedged root half costs the
/// command half a second.
pub const ROOT_HALF_LOOK: Duration = Duration::from_millis(500);

/// #4423 — what a person on a Mac can see of the root half, when `roomlerd
/// self-update` says what the helper will do with their ask. The helper reads
/// the root half over the system socket, whose folder refuses a person (0700
/// root), and asks it `live_work`, which the person socket never serves. What
/// a person may see is whether root serves them the person socket it keeps for
/// the console person (FR-97 P2g, `localapi::own_person_socket_path`): the
/// same process, and the evidence of a root half `roomler status` reads
/// (`localapi::other_daemon_socket`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootHalfSeen {
    /// Root serves this person's socket: a two-row or a one-row Mac.
    Runs,
    /// No socket, or one nobody serves: no root half serves this person, as on
    /// a per-user Mac. The helper's `not_running`, read the same way.
    NotRunning,
    /// Anything else: a server that is not root, any other error, or no
    /// answer within the bound.
    Unread,
}

/// #4423 — the person's look at the root half. `connect` connects and sends
/// nothing (the seam, as [`crate::one_row_home::read_live_work`]'s: the real
/// one is the person socket, kept only when root serves it), within `bound`.
/// It reads as the helper reads its own connect: `ENOENT` and `ECONNREFUSED`
/// are not running, and anything else that is not a connection is unread.
pub async fn root_half_seen<F>(connect: F, bound: Duration) -> RootHalfSeen
where
    F: std::future::Future<Output = std::io::Result<()>>,
{
    match tokio::time::timeout(bound, connect).await {
        Ok(Ok(())) => RootHalfSeen::Runs,
        Ok(Err(e))
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            RootHalfSeen::NotRunning
        }
        Ok(Err(_)) | Err(_) => RootHalfSeen::Unread,
    }
}

/// #4423 — the line `roomlerd self-update`, run as a person on a Mac, prints
/// once the daemon behind their own socket recorded their ask: what
/// [`helper_arm`] does with that ask on this Mac, by what the person could see
/// of the root half. With none, the ask installs at once over everything. With
/// one, it does too, unless the root half serves more than remote-control
/// sessions (`root_beyond_sessions`): then the ask lapses with [`ASK_FRESH`],
/// before the hour's recheck, and the update waits as for a plain wake
/// ([`NO_ASK_SAYS`]). A look that failed says the narrower line, which names
/// no wait: never a wait the helper may not keep.
pub fn ask_recorded_says(root: RootHalfSeen) -> &'static str {
    match root {
        RootHalfSeen::Runs => ASK_WAITS_FOR_THE_ROOT_HALF,
        RootHalfSeen::NotRunning | RootHalfSeen::Unread => ASK_INSTALLS_NOW,
    }
}

/// #4423 — [`ask_recorded_says`] with no root half to wait for, and the
/// narrower promise.
const ASK_INSTALLS_NOW: &str = "Your ask is recorded: the update installs now over every \
     remote-control session, and over every SSH connection, run, transfer and recording your \
     own roomlerd serves.";

/// #4423 — [`ask_recorded_says`] on a Mac with a root half: two rows, or one
/// row. The line is conditional on what the root half serves, so it holds
/// whatever the helper then reads of it.
const ASK_WAITS_FOR_THE_ROOT_HALF: &str = "Your ask is recorded: the update installs now over \
     every remote-control session, and over every SSH connection, run, transfer and recording \
     your own roomlerd serves, unless this Mac's root daemon serves an SSH connection, a run, a \
     transfer or a recording, does not answer, or saw its network move in the last 10 minutes. \
     In that case it installs when nothing on this Mac is in use, or within about seven hours \
     if something still is.";

/// #4423 — the line `roomlerd self-update`, run as a person on a Mac, prints
/// when no ask was recorded: the wake is a plain one, and [`helper_arm`]
/// takes #3429's rule on both halves, on every shape, until the force after
/// seven counted defers an hour apart.
pub const NO_ASK_SAYS: &str = "The update installs when nothing on this Mac is in use, or \
     within about seven hours if something still is.";

/// #4372 — the wake path on a Mac, as the root update helper reads it at the
/// start of a run and while it waits. The decisions are pure and compiled on
/// every OS; the reads and writes are unix, and only the Mac's helper calls
/// them (`updater::run_update_helper`). The content is never read.
pub mod wake {
    use std::path::{Path, PathBuf};

    /// The parts of what is at the wake path that a write, a touch, a mode
    /// change, a new link or a replacement changes.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Stamp {
        pub dev: u64,
        pub ino: u64,
        pub size: u64,
        pub mtime_ns: i128,
        pub ctime_ns: i128,
    }

    /// What kind of entry is at the wake path.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Kind {
        File,
        Folder,
        /// Neither: a link, or any other kind.
        Other,
    }

    /// What `lstat` read at the wake path. Never followed.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Entry {
        pub kind: Kind,
        pub owner: u32,
        /// The permission bits, `st_mode & 0o7777`.
        pub mode: u32,
        pub links: u64,
        pub stamp: Stamp,
    }

    /// The mode of the helper's own file: every local writer can wake it.
    pub const MODE: u32 = 0o666;

    impl Entry {
        /// The helper's own file: a regular file of `owner`'s, [`MODE`], one
        /// link.
        pub fn is_own(&self, owner: u32) -> bool {
            self.kind == Kind::File && self.owner == owner && self.mode == MODE && self.links == 1
        }
    }

    /// What the run's start does at the wake path, first, before every gate.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum StartStep {
        /// Nothing is there.
        Nothing,
        /// Remove what is there, unread.
        Remove,
    }

    impl StartStep {
        /// Whether the step changes the wake path. launchd's `WatchPaths`
        /// starts the job again for a change the run itself makes there.
        pub fn changes_path(&self) -> bool {
            matches!(self, StartStep::Remove)
        }
    }

    /// What the start does with what `lstat` found at the wake path. `owner`
    /// is the uid the helper runs as (root).
    pub fn start_step(found: &std::io::Result<Option<Entry>>, owner: u32) -> StartStep {
        let _ = owner;
        match found {
            Ok(None) => StartStep::Nothing,
            Ok(Some(_)) | Err(_) => StartStep::Remove,
        }
    }

    /// Whether a waiting helper reads a wake: a wake file is there.
    pub fn woke(baseline: Option<Stamp>, now: Option<Stamp>) -> bool {
        let _ = baseline;
        now.is_some()
    }

    /// A waiting helper's wake, read each tick ([`Watch::look`]).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Watch {
        path: PathBuf,
        baseline: Option<Stamp>,
        last: Option<Stamp>,
    }

    impl Watch {
        /// The wait's view of `path`, from what the run's start left there.
        pub fn new(path: &Path, baseline: Option<Stamp>) -> Watch {
            Watch {
                path: path.to_path_buf(),
                baseline,
                last: baseline,
            }
        }

        /// The wake path.
        pub fn path(&self) -> &Path {
            &self.path
        }

        /// One tick's read of the wake path: whether it is a wake. The read
        /// is kept for [`Watch::cycled`].
        pub fn look_at(&mut self, now: Option<Stamp>) -> bool {
            self.last = now;
            woke(self.baseline, now)
        }

        /// A cycle starts from the tick that decided it: the baseline becomes
        /// that tick's read, never a read taken after the cycle, so a write
        /// made during the cycle is the next tick's wake.
        pub fn cycled(&mut self) {
            self.baseline = self.last;
        }

        /// [`Watch::look_at`] with a read of the path now.
        #[cfg(unix)]
        pub fn look(&mut self) -> bool {
            let now = stamp_at(&self.path);
            self.look_at(now)
        }
    }

    #[cfg(unix)]
    impl Entry {
        /// The entry `lstat` describes.
        pub fn of(meta: &std::fs::Metadata) -> Entry {
            use std::os::unix::fs::MetadataExt as _;
            let ft = meta.file_type();
            let kind = if ft.is_file() {
                Kind::File
            } else if ft.is_dir() {
                Kind::Folder
            } else {
                Kind::Other
            };
            let ns = |secs: i64, nsec: i64| i128::from(secs) * 1_000_000_000 + i128::from(nsec);
            Entry {
                kind,
                owner: meta.uid(),
                mode: meta.mode() & 0o7777,
                links: meta.nlink(),
                stamp: Stamp {
                    dev: meta.dev(),
                    ino: meta.ino(),
                    size: meta.size(),
                    mtime_ns: ns(meta.mtime(), meta.mtime_nsec()),
                    ctime_ns: ns(meta.ctime(), meta.ctime_nsec()),
                },
            }
        }
    }

    /// What is at `path`, by `lstat`: `None` when nothing is.
    #[cfg(unix)]
    pub fn read(path: &Path) -> std::io::Result<Option<Entry>> {
        match std::fs::symlink_metadata(path) {
            Ok(meta) => Ok(Some(Entry::of(&meta))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The stamp of what is at `path`; `None` when nothing is, or when it
    /// cannot be read.
    #[cfg(unix)]
    pub fn stamp_at(path: &Path) -> Option<Stamp> {
        read(path).ok().flatten().map(|entry| entry.stamp)
    }

    /// The run's start, first, before every gate: removes what is at `path`,
    /// unread (`remove_file` never follows a link), and returns the wait's
    /// [`Watch`].
    #[cfg(unix)]
    pub fn settle(path: &Path, owner: u32) -> Watch {
        match start_step(&read(path), owner) {
            StartStep::Nothing => {}
            StartStep::Remove => {
                let _ = std::fs::remove_file(path);
            }
        }
        Watch::new(path, None)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn stamp(ino: u64) -> Stamp {
            Stamp {
                dev: 1,
                ino,
                size: 0,
                mtime_ns: 0,
                ctime_ns: 0,
            }
        }

        /// The helper's own file: root's, [`MODE`], one link.
        fn own() -> Entry {
            Entry {
                kind: Kind::File,
                owner: 0,
                mode: MODE,
                links: 1,
                stamp: stamp(7),
            }
        }

        #[test]
        fn a_file_where_there_was_none_is_a_wake_and_nothing_is_none() {
            assert!(woke(None, Some(stamp(1))));
            assert!(!woke(None, None));
        }

        #[test]
        fn the_start_removes_what_is_at_the_wake_path() {
            assert_eq!(start_step(&Ok(None), 0), StartStep::Nothing);
            let plain = Entry {
                mode: 0o644,
                ..own()
            };
            assert_eq!(start_step(&Ok(Some(plain)), 0), StartStep::Remove);
            let unread = Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
            assert_eq!(start_step(&unread, 0), StartStep::Remove);
        }

        /// #4372 — a change the start makes at the path is one more launchd
        /// run, a minute later, for every wake.
        #[test]
        fn the_start_makes_no_change_to_the_helpers_own_file() {
            assert!(!start_step(&Ok(Some(own())), 0).changes_path());
        }

        #[test]
        fn an_unchanged_wake_file_is_no_wake() {
            assert!(!woke(Some(stamp(7)), Some(stamp(7))));
        }

        /// After a cycle the baseline is the read of the tick that decided
        /// it, so a write made during the cycle is the next tick's wake.
        #[test]
        fn a_write_during_a_cycle_is_the_next_ticks_wake() {
            let mut watch = Watch::new(Path::new("/w"), Some(stamp(0)));
            assert!(watch.look_at(Some(stamp(1))), "a write since the start");
            watch.cycled();
            assert!(watch.look_at(Some(stamp(2))), "a write during that cycle");
            watch.cycled();
            assert!(!watch.look_at(Some(stamp(2))), "served by the second cycle");
        }

        #[cfg(unix)]
        #[test]
        fn the_read_never_follows_a_link() {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("target");
            std::fs::write(&target, b"bytes").unwrap();
            let wake = dir.path().join("wake");
            std::os::unix::fs::symlink(&target, &wake).unwrap();
            let entry = read(&wake).unwrap().unwrap();
            assert_eq!(entry.kind, Kind::Other);
            assert_ne!(entry.stamp, read(&target).unwrap().unwrap().stamp);
            assert_eq!(read(&dir.path().join("nothing")).unwrap(), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_795_900_000;
    const HOUR: u64 = 3600;

    fn answered(rc: u32, ssh: u32, exec: u32, work: u32, net: bool, ask: Option<u64>) -> Reading {
        Reading::Answered(LiveWorkInfo {
            rc_sessions: rc,
            ssh_connections: ssh,
            exec_runs: exec,
            work,
            net_transition: net,
            ask_age_secs: ask,
        })
    }
    fn idle() -> Reading {
        answered(0, 0, 0, 0, false, None)
    }
    fn session() -> Reading {
        answered(1, 0, 0, 0, false, None)
    }
    fn chain(defers: u32, next_due: u64) -> Budget {
        Budget { defers, next_due }
    }
    fn counted(defers: u32, next_due: u64) -> HelperArm {
        HelperArm::Defer {
            budget: chain(defers, next_due),
            counted: true,
        }
    }
    fn installs() -> HelperArm {
        HelperArm::Install {
            warning: None,
            forced: false,
        }
    }
    fn forced(what: &str) -> HelperArm {
        HelperArm::Install {
            warning: Some(format!("forced update proceeding despite: {what}")),
            forced: true,
        }
    }
    fn waiting(now: u64) -> Observed {
        Observed {
            awake: Duration::from_secs(HOUR),
            now,
            since_cycle: Duration::from_secs(HOUR),
            ..Observed::default()
        }
    }

    /// C1 — a session in the person's half, the root half not running, no
    /// chain: the first defer, rechecked in an hour.
    #[test]
    fn a_persons_session_defers_by_the_hour() {
        let arm = helper_arm(&Reading::NotRunning, &session(), Budget::RESET, NOW);
        assert_eq!(arm, counted(1, NOW + HOUR));
        assert_eq!(budget_for(&arm, Budget::RESET), chain(1, NOW + HOUR));
    }

    /// C2 — the root half's SSH connection, or its exec run, defers.
    #[test]
    fn the_root_halfs_ssh_or_exec_defers() {
        for root in [
            answered(0, 1, 0, 0, false, None),
            answered(0, 0, 1, 0, false, None),
        ] {
            let arm = helper_arm(&root, &idle(), Budget::RESET, NOW);
            assert_eq!(arm, counted(1, NOW + HOUR), "{root:?}");
        }
    }

    /// C3 — nothing live in either half (idle or not running) installs, and
    /// the install resets the budget.
    #[test]
    fn nothing_live_installs_and_resets() {
        let three = chain(3, NOW - 1);
        for (root, person) in [
            (idle(), idle()),
            (Reading::NotRunning, idle()),
            (idle(), Reading::NotRunning),
            (Reading::NotRunning, Reading::NotRunning),
        ] {
            let arm = helper_arm(&root, &person, three, NOW);
            assert_eq!(arm, installs(), "{root:?} {person:?}");
            assert_eq!(budget_for(&arm, three), Budget::RESET);
        }
    }

    /// C4 — eight due cycles with a session live, the budget read back from
    /// its file each time: seven defers, then the force, which names the work.
    #[test]
    fn seven_due_defers_then_the_force() {
        let mut budget = Budget::RESET;
        let mut now = NOW;
        for n in 1..=7 {
            let arm = helper_arm(&Reading::NotRunning, &session(), budget, now);
            assert_eq!(arm, counted(n, now + HOUR), "defer {n}");
            let kept = budget_for(&arm, budget);
            budget = Budget::parse(&kept.to_bytes()).expect("the file reads back");
            now = budget.next_due;
        }
        let arm = helper_arm(&Reading::NotRunning, &session(), budget, now);
        assert_eq!(arm, forced("1 remote-control session(s)"));
        assert_eq!(budget_for(&arm, budget), Budget::RESET);
    }

    /// C5 — a hundred wakes inside a chain with live work: every one defers,
    /// none is counted, and the budget is unchanged.
    #[test]
    fn a_hundred_wakes_never_reach_the_force() {
        let three = chain(3, NOW + 3000);
        for i in 0..100 {
            let arm = helper_arm(&Reading::NotRunning, &session(), three, NOW + i * 29);
            assert_eq!(
                arm,
                HelperArm::Defer {
                    budget: three,
                    counted: false
                },
                "wake {i}"
            );
            assert_eq!(budget_for(&arm, three), three);
        }
    }

    /// C6 — a wake inside a chain with both halves idle installs, and resets.
    #[test]
    fn a_wake_when_idle_installs_and_resets() {
        let three = chain(3, NOW + 3000);
        let arm = helper_arm(&idle(), &idle(), three, NOW);
        assert_eq!(arm, installs());
        assert_eq!(budget_for(&arm, three), Budget::RESET);
    }

    /// C7 — an ask the root half recorded 5 s ago (an operator's push)
    /// forces over a person's session, and says so.
    #[test]
    fn the_root_halfs_push_forces_over_a_persons_session() {
        let root = answered(0, 0, 0, 0, false, Some(5));
        let arm = helper_arm(&root, &session(), Budget::RESET, NOW);
        assert_eq!(arm, forced("1 remote-control session(s)"));
    }

    /// C8 — an ask the person's half recorded cannot cut the root half's SSH
    /// connection.
    #[test]
    fn a_persons_push_cannot_cut_the_root_half() {
        let person = answered(0, 0, 0, 0, false, Some(5));
        let root = answered(0, 1, 0, 0, false, None);
        assert_eq!(
            helper_arm(&root, &person, Budget::RESET, NOW),
            counted(1, NOW + HOUR)
        );
    }

    /// C9 — the person's half's ask waives that half's own session, with the
    /// root half not running.
    #[test]
    fn a_persons_push_waives_only_its_own_work() {
        let person = answered(1, 0, 0, 0, false, Some(5));
        let arm = helper_arm(&Reading::NotRunning, &person, Budget::RESET, NOW);
        assert_eq!(arm, forced("1 remote-control session(s)"));
    }

    /// C10 — an unread half defers (a wrong "idle" would cut a session); a
    /// stopped one does not.
    #[test]
    fn an_unread_half_defers_a_stopped_one_does_not() {
        for person in [Reading::Older, Reading::NoAnswer] {
            assert_eq!(
                helper_arm(&Reading::NotRunning, &person, Budget::RESET, NOW),
                counted(1, NOW + HOUR),
                "{person:?}"
            );
        }
        assert_eq!(
            helper_arm(
                &Reading::NotRunning,
                &Reading::NotRunning,
                Budget::RESET,
                NOW
            ),
            installs()
        );
    }

    /// C11 — R5b reads the root half's network transition alone: alone it
    /// rechecks in two minutes, beside a session in an hour, and a transition
    /// in the person's half alone gates nothing.
    #[test]
    fn r5b_reads_the_root_half_alone() {
        let moved = answered(0, 0, 0, 0, true, None);
        assert_eq!(
            helper_arm(&moved, &idle(), Budget::RESET, NOW),
            counted(1, NOW + 120)
        );
        assert_eq!(
            helper_arm(&moved, &session(), Budget::RESET, NOW),
            counted(1, NOW + HOUR)
        );
        assert_eq!(helper_arm(&idle(), &moved, Budget::RESET, NOW), installs());
    }

    /// C12 — an ask older than `ASK_FRESH` is a stale record: the cycle is
    /// gated as any other.
    #[test]
    fn a_stale_ask_is_a_wake() {
        let stale = Some(ASK_FRESH.as_secs() + 1);
        let root = answered(0, 0, 0, 0, false, stale);
        assert_eq!(
            helper_arm(&root, &session(), Budget::RESET, NOW),
            counted(1, NOW + HOUR)
        );
    }

    /// C13 — "up to date" resets the budget; a failed probe or download keeps
    /// it.
    #[test]
    fn up_to_date_resets_and_a_failed_probe_keeps() {
        let three = chain(3, NOW + 10);
        assert_eq!(
            budget_after_probe(ProbeSaid::UpToDate, three),
            Budget::RESET
        );
        assert_eq!(budget_after_probe(ProbeSaid::Failed, three), three);
    }

    /// C14 — a `next_due` further away than one recheck means the clock went
    /// back: the cycle is due, and counted.
    #[test]
    fn a_clock_set_back_makes_the_cycle_due() {
        let two = chain(2, NOW + 5 * HOUR);
        assert!(two.due(NOW));
        assert!(!chain(2, NOW + HOUR).due(NOW), "an hour ahead is a chain");
        assert_eq!(
            helper_arm(&Reading::NotRunning, &session(), two, NOW),
            counted(3, NOW + HOUR)
        );
    }

    /// C15 — the budget's file parses strictly; anything else is no chain.
    #[test]
    fn the_budget_file_parses_strictly() {
        let good = br#"{"v":1,"defers":3,"next_due":1795900000}"#;
        assert_eq!(Budget::parse(good), Some(chain(3, 1_795_900_000)));
        assert_eq!(
            Budget::parse(&chain(7, 9).to_bytes()),
            Some(chain(7, 9)),
            "the cap itself reads back"
        );
        let mut long = good.to_vec();
        long.resize(BUDGET_MAX_BYTES + 1, b' ');
        for bad in [
            br#"{"v":1,"defers":3,"next_due":1795900000,"x":1}"#.to_vec(),
            br#"{"v":2,"defers":3,"next_due":1795900000}"#.to_vec(),
            br#"{"v":1,"defers":8,"next_due":1795900000}"#.to_vec(),
            br#"{"v":1,"defers":4294967296,"next_due":1}"#.to_vec(),
            br#"{"v":1,"next_due":1}"#.to_vec(),
            b"not json".to_vec(),
            long,
        ] {
            assert_eq!(
                Budget::parse(&bad),
                None,
                "{}",
                String::from_utf8_lossy(&bad)
            );
        }
    }

    /// C16 — a waiting helper spaces wakes by the launchd gap, exits on the
    /// opt-out and on a replaced program, installs on the kill switch, and
    /// otherwise waits a tick until `next_due`.
    #[test]
    fn next_wake_spaces_wakes_and_exits_on_markers() {
        let one = chain(1, NOW + 3000);
        let wake = |since: u64| Observed {
            wake: true,
            since_cycle: Duration::from_secs(since),
            ..waiting(NOW)
        };
        assert_eq!(
            next_step(&wake(30), one),
            Step::Wait(Duration::from_secs(30))
        );
        assert_eq!(next_step(&wake(60), one), Step::Cycle);
        assert_eq!(next_step(&waiting(NOW), one), Step::Wait(TICK));
        assert_eq!(next_step(&waiting(NOW + 3000), one), Step::Cycle);
        let opted = Observed {
            opt_out: true,
            ..wake(60)
        };
        assert_eq!(next_step(&opted, one), Step::Exit(ExitWhy::OptedOut));
        let replaced = Observed {
            program_replaced: true,
            ..wake(60)
        };
        assert_eq!(next_step(&replaced, one), Step::Exit(ExitWhy::Replaced));
        let switched = Observed {
            kill_switch: true,
            ..wake(60)
        };
        assert_eq!(next_step(&switched, one), Step::InstallOverAll);
    }

    /// C17 — the kill switch installs with no reading, as before FR-151.
    #[test]
    fn the_kill_switch_installs_over_live_work() {
        assert!(!reads_before_install(true));
    }

    /// The ruling's ceiling: a waiting helper exits after 8 h of awake time,
    /// without installing; the budget stays for the next run.
    #[test]
    fn the_waiting_helper_ends_at_its_ceiling() {
        let one = chain(1, NOW + 3000);
        let almost = Observed {
            awake: AWAKE_CEILING - Duration::from_secs(1),
            ..waiting(NOW)
        };
        assert_eq!(next_step(&almost, one), Step::Wait(TICK));
        let over = Observed {
            awake: AWAKE_CEILING,
            ..waiting(NOW + 3000)
        };
        assert_eq!(next_step(&over, one), Step::Exit(ExitWhy::Ceiling));
    }

    /// The ruling: the budget in memory counts within a run, so a write that
    /// always fails still reaches the force.
    #[test]
    fn a_failed_budget_write_still_reaches_the_force() {
        let mut mem = Budget::RESET;
        let mut now = NOW;
        for n in 1..=7 {
            let HelperArm::Defer { budget, .. } =
                helper_arm(&Reading::NotRunning, &session(), mem, now)
            else {
                panic!("defer {n} expected");
            };
            let wrote = keep_budget(&mut mem, budget, |_| {
                Err(std::io::Error::other("no space left on device"))
            });
            assert!(wrote.is_err(), "the failed write is reported, to be logged");
            assert_eq!(mem, chain(n, now + HOUR), "the count advances anyway");
            now = mem.next_due;
        }
        assert_eq!(
            helper_arm(&Reading::NotRunning, &session(), mem, now),
            forced("1 remote-control session(s)")
        );
    }

    /// The ruling: SIGTERM ends a waiting helper at once and never installs,
    /// whatever else it sees at that tick.
    #[test]
    fn sigterm_ends_a_waiting_helper_without_installing() {
        let one = chain(1, NOW);
        let everything = Observed {
            kill_switch: true,
            wake: true,
            ..waiting(NOW)
        };
        assert_eq!(next_step(&everything, one), Step::InstallOverAll);
        let stopped = Observed {
            stop: true,
            ..everything
        };
        assert_eq!(next_step(&stopped, one), Step::Exit(ExitWhy::Stopped));
    }

    /// The ruling: an error reply counts as unread, so it defers.
    #[tokio::test]
    async fn an_error_reply_counts_as_unread() {
        let answer = crate::one_row_home::read_live_work(
            async {
                Err(std::io::Error::other(
                    "localapi error: live work is not reported on this node",
                ))
            },
            ASK_TIMEOUT,
        )
        .await;
        assert_eq!(answer.reading(), Reading::NoAnswer);
        assert_eq!(
            helper_arm(&Reading::NotRunning, &answer.reading(), Budget::RESET, NOW),
            counted(1, NOW + HOUR)
        );
    }

    /// The operator's answer: the person's "Apply update" installs over a
    /// session on their own desktop, on a per-user Mac (the person's half
    /// holds it) and on a one-row Mac (the root half holds it, and the GUI
    /// worker answers the person's socket).
    #[test]
    fn the_persons_click_installs_over_their_own_desktop() {
        let click = answered(0, 0, 0, 0, false, Some(3));
        let clicked_with_session = answered(1, 0, 0, 0, false, Some(3));
        assert_eq!(
            helper_arm(
                &Reading::NotRunning,
                &clicked_with_session,
                Budget::RESET,
                NOW
            ),
            forced("1 remote-control session(s)"),
            "per-user Mac"
        );
        assert_eq!(
            helper_arm(&session(), &click, chain(2, NOW + 100), NOW),
            forced("1 remote-control session(s)"),
            "one-row Mac"
        );
    }

    /// The operator's answer: the click never installs over the root half's
    /// SSH connection, exec run, transfer or recording, a network transition,
    /// or a root half it could not read.
    #[test]
    fn the_persons_click_never_cuts_the_root_halfs_work() {
        let click = answered(1, 0, 0, 0, false, Some(3));
        for root in [
            answered(0, 1, 0, 0, false, None),
            answered(0, 0, 1, 0, false, None),
            answered(0, 0, 0, 1, false, None),
            answered(0, 0, 0, 0, true, None),
            Reading::Older,
            Reading::NoAnswer,
        ] {
            assert!(
                matches!(
                    helper_arm(&root, &click, Budget::RESET, NOW),
                    HelperArm::Defer { counted: true, .. }
                ),
                "{root:?}"
            );
        }
    }

    /// The second reading after the download: a forced arm stands; an idle
    /// one that now reads busy defers, on the same budget and cycle.
    #[test]
    fn the_second_reading_can_drop_the_download() {
        let first = helper_arm(&idle(), &idle(), Budget::RESET, NOW);
        assert_eq!(first, installs());
        assert_eq!(
            second_arm(&first, &idle(), &session(), Budget::RESET, NOW),
            counted(1, NOW + HOUR)
        );
        let pushed = forced("1 remote-control session(s)");
        assert_eq!(
            second_arm(&pushed, &idle(), &session(), Budget::RESET, NOW),
            pushed
        );
    }

    /// The defer line names the work, the count, the wait, the cycle and each
    /// half's reading.
    #[test]
    fn the_defer_line_names_each_half() {
        let line = defer_line(
            &Reading::NotRunning,
            &session(),
            chain(1, NOW + HOUR),
            NOW,
            true,
        );
        assert_eq!(
            line,
            "busy=1 remote-control session(s) consecutive_defers=1 next_check_secs=3600 \
             cycle=due root=not_running person=answered"
        );
        assert_eq!(
            what_it_cuts(&Reading::Older, &idle()),
            "the root half, unread (older)"
        );
    }

    /// The line on a per-user Mac when the person's ask is recorded, and when
    /// the look at the root half failed. No word is read from the constants:
    /// a change to the words shows here.
    const INSTALLS_NOW: &str = "Your ask is recorded: the update installs now over every \
         remote-control session, and over every SSH connection, run, transfer and recording \
         your own roomlerd serves.";

    /// #4423 — a per-user Mac: no root half serves the person, the helper
    /// reads the root half `not_running`, and the ask installs at once over
    /// everything the person's own daemon serves, SSH connections and runs
    /// included, inside a chain or not. The helper's behaviour first, then
    /// the words.
    #[test]
    fn self_update_on_a_per_user_mac_says_it_installs_now() {
        let ask = answered(1, 1, 1, 1, false, Some(3));
        for budget in [Budget::RESET, chain(3, NOW + 100)] {
            assert_eq!(
                helper_arm(&Reading::NotRunning, &ask, budget, NOW),
                forced(
                    "1 remote-control session(s), 1 SSH connection(s), 1 exec run(s), \
                     1 transfer(s) or recording"
                ),
                "{budget:?}"
            );
        }
        assert_eq!(ask_recorded_says(RootHalfSeen::NotRunning), INSTALLS_NOW);
    }

    /// #4423 — a Mac with a root half (two rows, or one row): the ask
    /// installs over every remote-control session and over what the person's
    /// own daemon serves, unless the root half serves an SSH connection, a
    /// run, a transfer or a recording, is unread, or saw its network move.
    /// Then the ask lapses before the hour's recheck, and the update waits as
    /// for a plain wake, the person's own work included, until the force.
    /// The helper's behaviour first, then the words.
    #[test]
    fn self_update_on_a_mac_with_a_root_half_says_what_it_waits_for() {
        let ask = answered(1, 1, 1, 1, false, Some(3));
        assert!(
            matches!(
                helper_arm(&session(), &ask, Budget::RESET, NOW),
                HelperArm::Install { forced: true, .. }
            ),
            "the root half's session alone is installed over"
        );
        let waited_for = [
            answered(0, 1, 0, 0, false, None),
            answered(0, 0, 1, 0, false, None),
            answered(0, 0, 0, 1, false, None),
            answered(0, 0, 0, 0, true, None),
            Reading::Older,
            Reading::NoAnswer,
        ];
        for root in waited_for {
            assert!(
                matches!(
                    helper_arm(&root, &ask, Budget::RESET, NOW),
                    HelperArm::Defer { counted: true, .. }
                ),
                "{root:?}"
            );
            assert!(
                matches!(
                    helper_arm(&root, &ask, chain(MAX_CONSECUTIVE_DEFERS, NOW), NOW),
                    HelperArm::Install { forced: true, .. }
                ),
                "the budget's force, {root:?}"
            );
        }
        // In that case: at the hour's recheck the ask has lapsed, so the
        // person's own SSH connection holds the update once the root half's
        // work is gone, and a Mac with nothing live installs.
        let lapsed = |ssh| answered(0, ssh, 0, 0, false, Some(TRANSFER_DEFER_RECHECK.as_secs()));
        assert_eq!(
            helper_arm(&idle(), &lapsed(1), chain(1, NOW), NOW),
            counted(2, NOW + HOUR)
        );
        assert_eq!(
            helper_arm(&idle(), &lapsed(0), chain(1, NOW), NOW),
            installs()
        );
        assert_eq!(
            ask_recorded_says(RootHalfSeen::Runs),
            "Your ask is recorded: the update installs now over every remote-control session, \
             and over every SSH connection, run, transfer and recording your own roomlerd \
             serves, unless this Mac's root daemon serves an SSH connection, a run, a transfer \
             or a recording, does not answer, or saw its network move in the last 10 minutes. \
             In that case it installs when nothing on this Mac is in use, or within about seven \
             hours if something still is."
        );
    }

    /// #4423 — a root half the person could not read: the line is the
    /// narrower promise, the per-user Mac's, which promises no wait the
    /// helper may not keep.
    #[test]
    fn self_update_with_the_root_half_unread_promises_no_wait() {
        assert_eq!(ask_recorded_says(RootHalfSeen::Unread), INSTALLS_NOW);
    }

    /// #4423 — no ask recorded: a plain wake, which waits for both halves on
    /// #3429's budget, true on every shape; the force after seven counted
    /// defers ends that wait.
    #[test]
    fn self_update_with_no_ask_says_when_the_wait_ends() {
        assert_eq!(
            NO_ASK_SAYS,
            "The update installs when nothing on this Mac is in use, or within about seven \
             hours if something still is."
        );
    }

    /// #4423 — the numbers the lines name are the budget's: seven hourly
    /// counted defers before the force, and R5b's 10-minute window. "Now"
    /// holds because a wake is read within a minute, while the ask is fresh;
    /// "in that case" holds because the hour's recheck comes after it lapsed.
    #[test]
    fn the_lines_name_the_budgets_numbers() {
        assert_eq!(
            u64::from(MAX_CONSECUTIVE_DEFERS) * TRANSFER_DEFER_RECHECK.as_secs(),
            7 * HOUR
        );
        assert_eq!(
            crate::updater::NET_TRANSITION_WINDOW,
            Duration::from_secs(600)
        );
        assert!(UPDATE_HELPER_MIN_GAP < ASK_FRESH);
        assert!(TRANSFER_DEFER_RECHECK > ASK_FRESH);
    }

    /// #4423 — the person's look at the root half reads as the helper's own
    /// connect: a connection is a root half that runs; no socket, or one
    /// nobody serves, is none; a server that is not root (refused as
    /// `PermissionDenied`), or any other error, is unread.
    #[tokio::test]
    async fn a_persons_look_at_the_root_half_reads_as_the_helpers() {
        use std::io::{Error, ErrorKind};
        let look = |r: std::io::Result<()>| root_half_seen(std::future::ready(r), ROOT_HALF_LOOK);
        assert_eq!(look(Ok(())).await, RootHalfSeen::Runs);
        for kind in [ErrorKind::NotFound, ErrorKind::ConnectionRefused] {
            assert_eq!(
                look(Err(Error::from(kind))).await,
                RootHalfSeen::NotRunning,
                "{kind:?}"
            );
        }
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::UnexpectedEof,
            ErrorKind::Other,
        ] {
            assert_eq!(
                look(Err(Error::from(kind))).await,
                RootHalfSeen::Unread,
                "{kind:?}"
            );
        }
    }

    /// #4423 — a root half that never answers the look is unread within the
    /// bound (paused clock).
    #[tokio::test(start_paused = true)]
    async fn a_look_with_no_answer_is_unread() {
        assert_eq!(
            root_half_seen(std::future::pending(), ROOT_HALF_LOOK).await,
            RootHalfSeen::Unread
        );
    }
}

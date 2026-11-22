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
#[allow(unused_imports)] // the seam reads none of them
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
    #[allow(dead_code)] // the seam reads no ask
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
        // The seam (FR-151 P1, commit 1): today's helper keeps no budget.
        let _ = (bytes, BUDGET_MAX_BYTES);
        None
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
        // The seam: every cycle of today's helper is due.
        let _ = now;
        true
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
    // The seam: today's helper never reads.
    let _ = kill_switch;
    false
}

/// The arm of one cycle at `now`, from the two readings and the budget.
pub fn helper_arm(root: &Reading, person: &Reading, budget: Budget, now: u64) -> HelperArm {
    // The seam: today's helper installs whatever the halves serve.
    let _ = (root, person, budget, now);
    HelperArm::Install {
        warning: None,
        forced: false,
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
    // The seam: no second reading.
    let _ = (root, person, budget, now);
    first.clone()
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
    // The seam: today's helper keeps no budget.
    let _ = said;
    budget
}

/// Keep `new` as the budget in memory, then write it. The budget in memory is
/// the one that counts within a run, so a failed write (a full disk) cannot
/// keep a chain from the force; the error is returned to be logged.
pub fn keep_budget(
    mem: &mut Budget,
    new: Budget,
    write: impl FnOnce(&Budget) -> std::io::Result<()>,
) -> std::io::Result<()> {
    // The seam: today's helper keeps no budget.
    let _ = (mem, new, write);
    Ok(())
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
    // The seam: today's helper never waits; it exits after its one cycle.
    let _ = (seen, budget);
    Step::Exit(ExitWhy::Stopped)
}

/// What one cycle's arm leaves as the budget: an install resets it, just
/// before `installer(8)` runs; a defer keeps the one it carries.
pub fn budget_for(arm: &HelperArm, budget: Budget) -> Budget {
    // The seam (FR-151 P1, commit 1): today's helper keeps no budget.
    let _ = arm;
    budget
}

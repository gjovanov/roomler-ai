// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
// RETIRED-NAME-ANCHOR-BEGIN
// This whole module EXISTS to clean up pre-rename installs. Every retired
// name in it is something it must still recognise on a host that has one: the
// legacy SCM service `RoomlerAgentService`, the legacy Scheduled Task
// `RoomlerAgent`, the pre-P4b `roomler-agent` install folders, the
// `roomler-agent` app segment, the inert `roomler-agent.exe` alias and the
// `roomler_agent_exe` MSI FileKey.
// INVARIANT: a retired name here must be one this cleanup has to FIND. If you add
// one that is not, that is a bug, not a new exemption. docs/fr/FR-21
//! Cross-flavour install cleanup (Plan rc.18 P2).
//!
//! Removes stale artifacts left behind when an operator switches
//! between the perUser and perMachine MSI flavours of the agent.
//! Invoked by the MSI's custom action just before `InstallFiles` so
//! the new install lands cleanly.
//!
//! ## What gets cleaned
//!
//! When installing **perUser**, the helper cleans **perMachine**
//! leftovers:
//! - SCM service `RoomlerAgentService` (sc stop + sc delete) — needs
//!   admin; perUser MSI's CA runs Impersonated and inherits the user
//!   token, so this is best-effort. A non-admin user can't have a
//!   live perMachine install anyway, so failure here is benign.
//! - Service log dirs `%PROGRAMDATA%\roomler\roomler-agent\service-logs\`
//!   and the `roomler` segment's, removed by handle (FR-145, below).
//!
//! When installing **perMachine**, the helper cleans **perUser**
//! leftovers:
//! - #3801: the Scheduled Tasks of the account signed in to the step's
//!   session, and no other account's: its `Roomler-<SID>`, and `Roomler` or
//!   `RoomlerAgent` when their principal is that account
//!   (`service::user_task::clean_up_choice`). Another account's task, or one
//!   whose account it cannot read, stays, with a line that says so.
//! - The session user's data dirs at `AppData\Local\roomler\
//!   roomler-agent\` and `AppData\Roaming\roomler\roomler-agent\` (and the
//!   `roomler` segment; `needs-attention.txt` goes with the Roaming one),
//!   below the profile the system records for that account's SID
//!   (`user_profile::session_account_sid`, `appdirs::profile_of_account`),
//!   never a folder named after the user; none on the service account (#3702).
//! - FR-145: each tree is removed by handle, through the walk that checked
//!   it (`HeldPath::remove_tree`); a tree with a link on the way or in it is
//!   left in place, with a line that names the link.
//!
//! ## Vacated-dir sweep (P4b)
//!
//! P4b renamed the wxs `APPLICATIONFOLDER` from `roomler-agent` to
//! `Roomler`, so every MajorUpgrade from ≤rc.194 vacates the old
//! directory: the perMachine wxs has no `RemoveFolder` entries, and
//! the perUser flavour leaves PendingFileRename residue whenever the
//! running agent's EXE was locked during `RemoveFiles` (deleting the
//! Scheduled Task does not kill the already-running process). The
//! sweep removes known product files from the old-named directory
//! for the TARGET flavour's scope, then the directory itself if
//! empty. It runs BEFORE the same-flavour fast path — see below for
//! why that is the only placement the MSI CA ever executes.
//!
//! ## Same-flavour fast path — and what the MSI CA actually reaches
//!
//! When the new install matches the existing install's flavour, this
//! helper exits 0 immediately (after the vacated-dir sweep).
//! Same-flavour upgrades go through WiX MajorUpgrade and don't need
//! any cross-flavour scrubbing.
//!
//! NB the flavour probe is `updater::current_install_flavour()`,
//! which classifies **this process's own exe path**. The MSI custom
//! actions shell the freshly-laid TARGET exe
//! (`FileKey='roomler_agent_exe'`), so in CA context current ==
//! target ALWAYS and the fast path fires on every CA invocation —
//! including genuine cross-flavour switches. The cross-flavour arms
//! below are therefore reachable ONLY via an operator manually
//! running `cleanup-legacy-install` from an oppositely-scoped exe;
//! the vacated-dir sweep is the single step with MSI-CA reach.
//! (Switching the probe to the registry-based `install_detect` would
//! make the arms CA-reachable but would also activate the user-data
//! -dir deletion — config + enrollment trees — on fleet flows;
//! deliberately NOT done in P4b.)
//!
//! ## Why a separate CLI subcommand
//!
//! WiX custom actions are easier to plumb when they shell out to a
//! deterministic exe. The CA does:
//!
//! ```text
//! "[INSTALLDIR]roomler-agent.exe" cleanup-legacy-install \
//!     --target-flavour {perUser|perMachine}
//! ```
//!
//! Return="ignore" on the CA so a cleanup failure doesn't sink the
//! whole install. We log + continue.
//!
//! ## Dry-run mode
//!
//! `--dry-run` prints what WOULD be removed without touching anything.
//! Used during MSI build smoke tests to validate the helper's logic
//! before flipping to the live path.

use anyhow::Result;
// Only the Windows cleanup paths name a path — gate the import behind cfg so
// Linux/macOS clippy doesn't flag it as unused.
#[cfg(target_os = "windows")]
use std::path::Path;

/// Which MSI flavour is being INSTALLED. The helper cleans the
/// OPPOSITE flavour's artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetFlavour {
    PerUser,
    PerMachine,
}

impl TargetFlavour {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "peruser" | "per-user" | "user" => Some(Self::PerUser),
            "permachine" | "per-machine" | "machine" => Some(Self::PerMachine),
            _ => None,
        }
    }
}

/// Tally of what cleanup did (or would do, when `dry_run=true`).
/// Each `Vec<String>` is a list of human-readable artifact descriptions
/// — surfaced via the CLI's one-line summary print.
#[derive(Debug, Default)]
pub struct CleanupReport {
    pub removed: Vec<String>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

impl CleanupReport {
    pub fn summary(&self) -> String {
        format!(
            "cleanup-legacy-install: removed {} skipped {} errors {} ({})",
            self.removed.len(),
            self.skipped.len(),
            self.errors.len(),
            self.removed
                .iter()
                .chain(self.skipped.iter())
                .chain(self.errors.iter())
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
        )
    }
}

/// Run cross-flavour cleanup for the given target flavour. When `dry_run`
/// is true, prints intent but doesn't mutate anything.
///
/// On non-Windows hosts this is a no-op returning an empty report —
/// only the Windows MSIs have cross-flavour ambiguity.
pub fn run_cleanup(target: TargetFlavour, dry_run: bool) -> Result<CleanupReport> {
    let mut report = CleanupReport::default();

    #[cfg(target_os = "windows")]
    {
        // P4b: sweep the vacated pre-rename install dir FIRST. The
        // same-flavour fast path below fires on EVERY MSI CA
        // invocation (see the module docs), so anything placed after
        // it never runs in CA context — this sweep is the one step
        // the installer actually reaches.
        cleanup_vacated_install_dir(target, &mut report, dry_run);

        // Same-flavour fast path: the new install matches what's
        // already on disk → no cross-flavour cleanup needed.
        let current = crate::updater::current_install_flavour();
        let cross_flavour_needed = !matches!(
            (current, target),
            (
                crate::updater::WindowsInstallFlavour::PerUser,
                TargetFlavour::PerUser,
            ) | (
                crate::updater::WindowsInstallFlavour::PerMachine,
                TargetFlavour::PerMachine,
            )
        );
        if !cross_flavour_needed {
            report
                .skipped
                .push("same-flavour install — no cross-flavour cleanup".to_string());
            return Ok(report);
        }
        match target {
            TargetFlavour::PerUser => cleanup_per_machine_artifacts(&mut report, dry_run),
            TargetFlavour::PerMachine => cleanup_per_user_artifacts(&mut report, dry_run),
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = (target, dry_run);
        report
            .skipped
            .push("non-Windows host — no cross-flavour cleanup".to_string());
    }

    Ok(report)
}

/// Filenames the MSIs have ever shipped into the install folder,
/// plus the tunnel CLI pair. The vacated-dir sweep deletes ONLY
/// these — never arbitrary files an operator parked there. The CLI
/// pair (`roomler.exe` / `roomler-tunnel.exe`) is included even
/// though pre-P4b MSIs never shipped it: in the OLD dir those can
/// only be manually-copied tunnel CLIs (field-observed), they are
/// OUR binaries in OUR dead directory, and a stale copy in a
/// manually-PATH'd old dir is a version-skew hazard once the new
/// dir's `roomler.exe` is authoritative.
#[cfg(target_os = "windows")]
const INSTALL_DIR_PRODUCT_FILES: &[&str] = &[
    "roomlerd.exe",
    "roomlerdw.exe",
    "roomler-agent.exe",
    "roomler.exe",
    "roomler-tunnel.exe",
    // S1a — the desktop companion + its refresh sidecars live beside the
    // daemon (placed by the wizard / companion-refresh, not the MSI);
    // without these entries the vacated-dir sweep strands a stale copy.
    "roomler-desktop.exe",
    "roomler-desktop.exe.version",
    "roomler-desktop.exe.old",
    "wintun.dll",
    "LICENSE.txt",
    "README.txt",
];

/// P4b: sweep the VACATED pre-rename install directory for the
/// TARGET flavour's scope (perUser →
/// `%LOCALAPPDATA%\Programs\roomler-agent`, perMachine →
/// `%ProgramFiles%\roomler-agent`). #3702 — both folders as the system
/// names them (FR-145: below the profile the system records for this
/// account's SID, or the machine's program files folder), never from the
/// environment; the sweep deletes nothing through a link.
///
/// Policy: conservative. Skip outright when the candidate IS the
/// directory this process runs from (paranoia guard — impossible
/// with the renamed wxs, but cheap insurance against a rebuilt
/// old-name MSI invoking a new exe). Delete only
/// [`INSTALL_DIR_PRODUCT_FILES`], then the folder NON-recursively:
/// foreign content survives and is reported; locked files fail soft
/// (the CA runs `Return='ignore'`; PendingFileRenameOperations
/// clears them on the next reboot, after which a later invocation
/// removes the then-empty dir).
///
/// FR-145: the folder is reached by handle from the base it lives below (the
/// machine's `Program Files` for a per-Machine install; for a per-User one,
/// the profile the system records for this account's SID), each folder on the
/// way opened below the one above it and never through a link; the product
/// files are deleted below the held folder, and the folder through the held
/// folder above it.
#[cfg(target_os = "windows")]
fn cleanup_vacated_install_dir(target: TargetFlavour, report: &mut CleanupReport, dry_run: bool) {
    let name = crate::updater::LEGACY_INSTALL_FOLDER_NAME;
    let base = match target {
        TargetFlavour::PerMachine => crate::appdirs::machine_program_files(),
        TargetFlavour::PerUser => match crate::win_token::own_user_sid()
            .and_then(|sid| crate::appdirs::profile_of_account(&sid))
        {
            Some(profile) => profile,
            None => {
                report.skipped.push(
                    "vacated-dir sweep: the system records no profile for this account".to_string(),
                );
                return;
            }
        },
    };
    let dir = match target {
        TargetFlavour::PerMachine => base.join(name),
        TargetFlavour::PerUser => base
            .join("AppData")
            .join("Local")
            .join("Programs")
            .join(name),
    };
    sweep_vacated_dir(&base, &dir, report, dry_run);
}

/// The sweep of one vacated install folder, `dir`, below `base`: what
/// [`cleanup_vacated_install_dir`] does once it has named it. A `dir` that
/// is a folder link, or sits below one, is left as it is, and so is what the
/// link leads to.
#[cfg(target_os = "windows")]
fn sweep_vacated_dir(base: &Path, dir: &Path, report: &mut CleanupReport, dry_run: bool) {
    use roomler_node_core::machine_tree::{HeldPath, Why};
    let left = |report: &mut CleanupReport, why: &dyn std::fmt::Display| {
        report.skipped.push(format!(
            "vacated dir {} left in place: {why}",
            dir.display()
        ));
    };
    let (Some(parent), Some(name)) = (dir.parent(), dir.file_name().and_then(|n| n.to_str()))
    else {
        return left(report, &"it names no folder");
    };
    // The folder above it, held by handle from `base`.
    let above = if parent == base {
        HeldPath::base(base).map(Some)
    } else {
        HeldPath::below(base, parent)
    };
    let held = match above.and_then(|above| match above {
        Some(above) => above
            .open_dir(name)
            .map(|held| held.map(|held| (above, held))),
        None => Ok(None),
    }) {
        Ok(Some(held)) => held,
        Ok(None) => {
            report
                .skipped
                .push(format!("vacated dir {} not present", dir.display()));
            return;
        }
        Err(refused) => return left(report, &refused),
    };
    let (above, held) = held;
    // Paranoia guard: never sweep the directory we are running from.
    if let Ok(own) = std::env::current_exe()
        && let Some(own_dir) = own.parent()
        && same_dir(own_dir, dir)
    {
        report.skipped.push(format!(
            "vacated-dir sweep: {} is the running exe's own directory — skipped",
            dir.display()
        ));
        return;
    }
    if dry_run {
        report.removed.push(format!(
            "[dry-run] sweep vacated install dir {} (known product files + rmdir-if-empty)",
            dir.display()
        ));
        return;
    }
    for file in INSTALL_DIR_PRODUCT_FILES {
        let path = dir.join(file);
        match held.remove_file(file) {
            Ok(true) => report.removed.push(format!("stale {}", path.display())),
            Ok(false) => {}
            Err(refused) if matches!(refused.why, Why::Link) => left(report, &refused),
            Err(refused) => {
                report
                    .errors
                    .push(format!("remove {}: {}", path.display(), refused.why))
            }
        }
    }
    drop(held);
    match above.remove_empty_dir(name) {
        Ok(true) => report
            .removed
            .push(format!("vacated install dir {}", dir.display())),
        Ok(false) => report.skipped.push(format!(
            "vacated dir {} left in place (non-product files or locks)",
            dir.display()
        )),
        Err(refused) => left(report, &refused),
    }
}

/// Case-insensitive same-directory check (Windows path semantics).
/// Falls back to lossy-string comparison when canonicalize fails —
/// the caller uses this as a skip-guard, so a false POSITIVE (skip
/// the sweep) is the safe failure mode.
#[cfg(target_os = "windows")]
fn same_dir(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy()),
    }
}

// ──────────────────────────────────────────────────────────────────
// perMachine → cleanup perUser artifacts (we are installing perMachine
// over a prior perUser install)
// ──────────────────────────────────────────────────────────────────
#[cfg(target_os = "windows")]
fn cleanup_per_user_artifacts(report: &mut CleanupReport, dry_run: bool) {
    // 1. Scheduled Tasks — registered by the perUser MSI's
    //    `RegisterAutostart` custom action: `RoomlerAgent` pre-P3d,
    //    `Roomler` since the P3d rename, `Roomler-<SID>` since #3801. Only
    //    those of the account this arm cleans: every account's install
    //    held the shared names, so one is removed only when its principal
    //    is that account. NB reachable via operator CLI only — the MSI CA
    //    always exits through the same-flavour fast path (module docs) — so
    //    this can never race the SAME install's freshly-registered task.
    let cleaned = account_cleaned();
    clean_up_user_tasks(
        &mut crate::service::Schtasks,
        cleaned.as_deref().map_err(|why| *why),
        report,
        dry_run,
    );

    // 2. The data dirs below the profile the system records for the
    //    session's account (FR-145: by its SID, never by its user name), as
    //    `plan_per_user_removal` decides (#3702).
    #[cfg(feature = "system-context")]
    {
        use crate::system_context::user_profile;
        let plan = plan_per_user_removal(
            crate::win_token::own_user_sid().as_deref(),
            || match user_profile::session_user_name() {
                Some(name) => SessionAccount::User {
                    name,
                    sid: user_profile::session_account_sid(),
                },
                None => SessionAccount::Nobody,
            },
            crate::appdirs::profile_of_account,
        );
        remove_per_user_trees(plan, report, dry_run);
    }

    // Without the system-context feature compiled in, the session's
    // account cannot be read; the cleanup becomes a
    // perUser-self-deletes-its-own-dirs operation. Since perMachine MSI
    // normally has the feature on, this branch only matters for the
    // default build used in headless CI tests.
    #[cfg(not(feature = "system-context"))]
    {
        report
            .skipped
            .push("system-context feature off — user data dir reach-through skipped".to_string());
    }
}

/// #3801 — the account the per-user arm cleans: the one signed in to the
/// step's session, whose data trees it removes too ([`plan_per_user_removal`]).
/// `Err`: why it cannot tell that account, and so removes no Scheduled Task.
#[cfg(target_os = "windows")]
fn account_cleaned() -> Result<String, &'static str> {
    #[cfg(feature = "system-context")]
    {
        use crate::system_context::user_profile;
        if user_profile::session_user_name().is_none() {
            return Err("nobody is signed in to this step's session");
        }
        user_profile::session_account_sid()
            .ok_or("this step cannot tell the account signed in to its session")
    }
    #[cfg(not(feature = "system-context"))]
    Err("this build cannot read the account signed in to the step's session")
}

/// #3801 — the per-user arm's Scheduled Tasks: those of `cleaned`, the account
/// it cleans ([`account_cleaned`]; `Err`: why it cannot tell that account),
/// and no other account's. The choice is `user_task::clean_up_choice`; every
/// task it finds has its line in `report`, and no line names an account.
#[cfg(target_os = "windows")]
fn clean_up_user_tasks(
    lib: &mut impl crate::service::user_task::Library,
    cleaned: Result<&str, &str>,
    report: &mut CleanupReport,
    dry_run: bool,
) {
    use crate::service::user_task::{Verdict, clean_up_choice};
    for choice in clean_up_choice(lib, cleaned) {
        let shown = &choice.shown;
        match choice.verdict {
            Verdict::Remove if dry_run => report
                .removed
                .push(format!("[dry-run] schtasks /Delete /TN {shown} /F")),
            Verdict::Remove => match lib.delete(&choice.name) {
                Ok(()) => report.removed.push(format!("Scheduled Task {shown}")),
                Err(e) => report.errors.push(format!("schtasks /Delete {shown}: {e}")),
            },
            Verdict::Absent => report
                .skipped
                .push(format!("Scheduled Task {shown} not present")),
            Verdict::Another => report.skipped.push(format!(
                "Scheduled Task {shown} left in place: it runs as another account"
            )),
            Verdict::Unknown => report.skipped.push(format!(
                "Scheduled Task {shown} left in place: this step cannot tell which account it \
                 runs as"
            )),
            Verdict::NoAccount(why) => report.skipped.push(format!(
                "Scheduled Task {shown} left in place: {why}, so this step removes no account's \
                 task"
            )),
        }
    }
}

/// #3702 — what the per-user clean-up does with a person's data trees: the
/// profile whose trees it removes, or the line that says why it removes none.
/// FR-145: each tree is checked and removed by the same walk, at removal
/// ([`remove_per_user_trees`]).
#[cfg(all(target_os = "windows", feature = "system-context"))]
#[derive(Debug, Default)]
struct PerUserPlan {
    profile: Option<std::path::PathBuf>,
    left: Vec<String>,
}

/// The account signed in to this process's session, as the per-user clean-up
/// reads it.
#[cfg(all(target_os = "windows", feature = "system-context"))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionAccount {
    /// Nobody is signed in to the session, or its user cannot be read.
    Nobody,
    /// The session's user: its name, and its account's SID when it could be
    /// told (`session_account_sid`).
    User { name: String, sid: Option<String> },
}

/// The app segments a cross-flavour switch scrubs: the legacy `roomler-agent`
/// one, from before the binary rename, and today's `roomler` (`appdirs.rs`).
/// Both are named here rather than routed through `appdirs`, which resolves
/// to only ONE segment per host.
#[cfg(target_os = "windows")]
const SEGMENTS: [&str; 2] = ["roomler-agent", "roomler"];

/// The data trees below `profile_root`, each as (area, segment): BOTH the
/// legacy `roomler-agent` app segment AND the new `roomler` segment, local
/// and roaming. A cross-flavour switch must scrub whichever segment the OTHER
/// flavour landed on.
#[cfg(all(target_os = "windows", feature = "system-context"))]
const PER_USER_TREES: [(&str, &str); 4] = [
    ("Local", "roomler-agent"),
    ("Roaming", "roomler-agent"),
    ("Local", "roomler"),
    ("Roaming", "roomler"),
];

/// The data trees below `profile_root` ([`PER_USER_TREES`]), as paths.
#[cfg(all(target_os = "windows", feature = "system-context"))]
fn per_user_trees(profile_root: &Path) -> [std::path::PathBuf; 4] {
    PER_USER_TREES.map(|(area, segment)| {
        profile_root
            .join("AppData")
            .join(area)
            .join("roomler")
            .join(segment)
    })
}

/// #3702 — the plan for the data trees ([`per_user_trees`]) below the
/// profile of the person signed in to this process's session. When the step
/// cannot read the SID of the account it runs as (`None`), or that account is
/// the service account, it removes none and asks `session` nothing. FR-145:
/// the profile is the one the system records for the session's account
/// (`profile_of` its SID, `appdirs::profile_of_account`), never a folder named
/// after its user; with none recorded, or an account it cannot tell, nothing
/// is removed and the plan's line says which.
#[cfg(all(target_os = "windows", feature = "system-context"))]
fn plan_per_user_removal(
    own_sid: Option<&str>,
    session: impl FnOnce() -> SessionAccount,
    profile_of: impl FnOnce(&str) -> Option<std::path::PathBuf>,
) -> PerUserPlan {
    let mut plan = PerUserPlan::default();
    let Some(own_sid) = own_sid else {
        plan.left.push(
            "user data dirs left in place: this step cannot tell which account it runs as, so \
             it looks up no profile and removes no user data dir; the Scheduled Task clean-up \
             still runs"
                .to_string(),
        );
        return plan;
    };
    if crate::appdirs::is_service_account(Some(own_sid)) {
        plan.left.push(
            "user data dirs left in place: on the service account this step looks up no \
             profile and removes no user data dir; the Scheduled Task clean-up still runs"
                .to_string(),
        );
        return plan;
    }
    match session() {
        SessionAccount::Nobody => plan
            .left
            .push("no active interactive session — user data dirs not reachable".to_string()),
        SessionAccount::User { sid: None, .. } => plan.left.push(
            "user data dirs left in place: cannot tell the session's account, so no profile is \
             looked up and no user data dir removed"
                .to_string(),
        ),
        SessionAccount::User { sid: Some(sid), .. } => match profile_of(&sid) {
            Some(profile) => plan.profile = Some(profile),
            None => plan.left.push(
                "user data dirs left in place: the system records no profile for the session's \
                 account, so no user data dir is removed"
                    .to_string(),
            ),
        },
    }
    plan
}

/// Carry out `plan`: report why it removes nothing, or remove each data tree
/// of its profile (`dry_run`: only say so). FR-145: each tree is reached by
/// handle from the profile (`AppData\<area>\roomler`, each folder opened
/// below the one above it, never through a link) and removed by
/// `HeldPath::remove_tree` below the folder that walk holds: the walk that
/// finds no link is the walk whose handles delete. A tree with a link on the
/// way or in it is left in place, with a line that names the link.
#[cfg(all(target_os = "windows", feature = "system-context"))]
fn remove_per_user_trees(plan: PerUserPlan, report: &mut CleanupReport, dry_run: bool) {
    use roomler_node_core::machine_tree::HeldPath;
    report.skipped.extend(plan.left);
    let Some(profile) = plan.profile else {
        return;
    };
    let base = HeldPath::base(&profile);
    for ((area, segment), tree) in PER_USER_TREES.into_iter().zip(per_user_trees(&profile)) {
        let removal = match &base {
            Ok(base) => base.walk(&["AppData", area, "roomler"]),
            Err(refused) => Err(refused.clone()),
        }
        .and_then(|held| match held {
            Some(held) => held.remove_tree(segment, dry_run).map(Some),
            None => Ok(None),
        });
        report_removal("user data dir", &tree, removal, dry_run, report);
    }
}

/// One tree's report lines, from what its walk and `remove_tree` did:
/// `"<what> <tree>"` when removed (`"[dry-run] rmdir /s <tree>"` when it
/// would be), `"<what> <tree> not present"`, `"<what> <tree> left in place:
/// <path> is a link …"` for each link that kept it (or the refusal that
/// stopped the walk), and an error line for anything that could not be
/// removed for another reason.
#[cfg(target_os = "windows")]
fn report_removal(
    what: &str,
    tree: &Path,
    removal: Result<
        Option<roomler_node_core::machine_tree::TreeRemoval>,
        roomler_node_core::machine_tree::held::Refusal,
    >,
    dry_run: bool,
    report: &mut CleanupReport,
) {
    use roomler_node_core::machine_tree::Why;
    let shown = tree.display();
    let removal = match removal {
        Ok(Some(removal)) if removal.present => removal,
        Ok(_) => return report.skipped.push(format!("{what} {shown} not present")),
        Err(refused) => {
            return report
                .skipped
                .push(format!("{what} {shown} left in place: {refused}"));
        }
    };
    if dry_run && removal.would_remove() {
        return report.removed.push(format!("[dry-run] rmdir /s {shown}"));
    }
    if removal.root_removed {
        return report.removed.push(format!("{what} {shown}"));
    }
    for (at, why) in removal.left {
        match why {
            Why::Io(e) => report.errors.push(format!("rmdir {}: {e}", at.display())),
            why => report.skipped.push(format!(
                "{what} {shown} left in place: {} {why}",
                at.display()
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────
// perUser → cleanup perMachine artifacts (we are installing perUser
// over a prior perMachine install)
// ──────────────────────────────────────────────────────────────────
#[cfg(target_os = "windows")]
fn cleanup_per_machine_artifacts(report: &mut CleanupReport, dry_run: bool) {
    // 1. SCM service: `RoomlerAgentService` pre-P3d, `Roomler` since
    //    the P3d rename (P4b hygiene: both scrubbed). The perUser
    //    MSI's CA runs Impersonated (user token). Service control
    //    requires admin — so this is BEST-EFFORT. If the user isn't
    //    admin, sc fails and we surface it as an error (the operator
    //    must manually `sc delete <name>` from elevated PS). NB
    //    reachable via operator CLI only — the MSI CA always exits
    //    through the same-flavour fast path (module docs).
    for service_name in ["RoomlerAgentService", "Roomler"] {
        if service_exists(service_name) {
            if dry_run {
                report
                    .removed
                    .push(format!("[dry-run] sc stop + sc delete {service_name}"));
            } else {
                // stop is best-effort (service may already be stopped).
                let _ = run_quiet("sc", &["stop", service_name]);
                match run_quiet("sc", &["delete", service_name]) {
                    Ok(()) => report.removed.push(format!("SCM service {service_name}")),
                    Err(e) => report.errors.push(format!("sc delete {service_name}: {e}")),
                }
            }
        } else {
            report
                .skipped
                .push(format!("SCM service {service_name} not present"));
        }
    }

    // 2. Service log dir at %PROGRAMDATA%\roomler\<segment>\service-logs\.
    cleanup_per_machine_service_logs(&crate::appdirs::machine_program_data(), report, dry_run);
}

/// The per-Machine arm's service log folders, below `program_data`, the
/// machine's `ProgramData` (a test names a folder of its own).
///
/// ACL'd to SYSTEM at create time; the perUser MSI's user-token CA may not be
/// able to delete it. Scrub BOTH [`SEGMENTS`] (binary rename, `appdirs.rs`) so
/// a cross-flavour switch doesn't leave the other segment's tree behind. Not
/// routed through `machine_global_dir` (which resolves to only ONE segment
/// per host); rooted at the machine's folder as the system names it.
///
/// FR-145: `program_data` is held by handle, `roomler\<segment>` walked below
/// it (never through a link), and `service-logs` removed by
/// `HeldPath::remove_tree` below the segment folder that walk holds. A link on
/// the way or in the tree leaves it, with a line that names the link.
#[cfg(target_os = "windows")]
fn cleanup_per_machine_service_logs(
    program_data: &Path,
    report: &mut CleanupReport,
    dry_run: bool,
) {
    use roomler_node_core::machine_tree::HeldPath;
    let base = HeldPath::base(program_data);
    for segment in SEGMENTS {
        let path = program_data
            .join("roomler")
            .join(segment)
            .join("service-logs");
        let removal = match &base {
            Ok(base) => base.walk(&["roomler", segment]),
            Err(refused) => Err(refused.clone()),
        }
        .and_then(|held| match held {
            Some(held) => held.remove_tree("service-logs", dry_run).map(Some),
            None => Ok(None),
        });
        report_removal("service log dir", &path, removal, dry_run, report);
    }
}

#[cfg(target_os = "windows")]
fn service_exists(name: &str) -> bool {
    std::process::Command::new("sc")
        .args(["query", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
fn run_quiet(cmd: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if !status.success() {
        anyhow::bail!("{cmd} {:?} exited {:?}", args, status.code());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_flavour_parses_friendly_strings() {
        assert_eq!(
            TargetFlavour::parse("perUser"),
            Some(TargetFlavour::PerUser)
        );
        assert_eq!(
            TargetFlavour::parse("PERUSER"),
            Some(TargetFlavour::PerUser)
        );
        assert_eq!(
            TargetFlavour::parse("per-user"),
            Some(TargetFlavour::PerUser)
        );
        assert_eq!(TargetFlavour::parse("user"), Some(TargetFlavour::PerUser));
        assert_eq!(
            TargetFlavour::parse("perMachine"),
            Some(TargetFlavour::PerMachine)
        );
        assert_eq!(
            TargetFlavour::parse("per-machine"),
            Some(TargetFlavour::PerMachine)
        );
        assert_eq!(
            TargetFlavour::parse("machine"),
            Some(TargetFlavour::PerMachine)
        );
        assert_eq!(TargetFlavour::parse("bogus"), None);
        assert_eq!(TargetFlavour::parse(""), None);
    }

    #[test]
    fn cleanup_report_summary_includes_counts_and_items() {
        let mut r = CleanupReport::default();
        r.removed.push("task RoomlerAgent".to_string());
        r.skipped.push("nothing else".to_string());
        let s = r.summary();
        assert!(s.contains("removed 1"));
        assert!(s.contains("skipped 1"));
        assert!(s.contains("errors 0"));
        assert!(s.contains("task RoomlerAgent"));
        assert!(s.contains("nothing else"));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn run_cleanup_on_non_windows_is_noop() {
        let report = run_cleanup(TargetFlavour::PerUser, true).unwrap();
        assert!(report.removed.is_empty());
        assert!(report.errors.is_empty());
        assert!(report.skipped.iter().any(|s| s.contains("non-Windows")));
    }

    // ── P4b vacated-dir sweep ─────────────────────────────────────

    /// The sweep targets the LEGACY folder name under the flavour's
    /// root: the account's own `LocalAppData` known folder for perUser,
    /// the machine's `Program Files` as the system names it for
    /// perMachine. Both are named on every Windows host.
    #[cfg(target_os = "windows")]
    #[test]
    fn vacated_dir_derives_legacy_name_per_flavour() {
        use crate::updater::{
            LEGACY_INSTALL_FOLDER_NAME, WindowsInstallFlavour, install_dir_with_name,
        };
        let pu = install_dir_with_name(WindowsInstallFlavour::PerUser, LEGACY_INSTALL_FOLDER_NAME)
            .expect("the account's LocalAppData folder");
        assert!(
            pu.ends_with(std::path::Path::new("Programs").join("roomler-agent")),
            "unexpected perUser dir {}",
            pu.display()
        );
        let pm = install_dir_with_name(
            WindowsInstallFlavour::PerMachine,
            LEGACY_INSTALL_FOLDER_NAME,
        )
        .expect("the machine's Program Files folder");
        assert_eq!(
            pm,
            crate::appdirs::machine_program_files().join("roomler-agent")
        );
        assert!(
            pm.to_string_lossy()
                .to_lowercase()
                .contains("program files"),
            "perMachine dir must live under Program Files: {}",
            pm.display()
        );
    }

    /// A junction at `link` that leads to the folder `target` (`mklink /J`,
    /// which needs no privilege).
    #[cfg(target_os = "windows")]
    fn junction(link: &Path, target: &Path) {
        let status = std::process::Command::new(crate::appdirs::system32_tool("cmd.exe"))
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J {}", link.display());
    }

    /// #3702 — the vacated-dir sweep deletes nothing through a link: a folder
    /// that is a junction, or sits below one, is left as it is, and so is
    /// everything the link leads to.
    #[cfg(target_os = "windows")]
    #[test]
    fn the_vacated_dir_sweep_follows_no_link() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir_all(real.join("roomler-agent")).unwrap();
        for file in [
            real.join("roomlerd.exe"),
            real.join("roomler-agent").join("roomlerd.exe"),
        ] {
            std::fs::write(&file, "x").unwrap();
        }
        // The folder itself is the link.
        let at = root.path().join("roomler-agent");
        junction(&at, &real);
        // The folder sits below one.
        let parent = root.path().join("parent");
        junction(&parent, &real);
        for dir in [at, parent.join("roomler-agent")] {
            let mut report = CleanupReport::default();
            sweep_vacated_dir(root.path(), &dir, &mut report, false);
            assert!(real.join("roomlerd.exe").is_file(), "{report:?}");
            assert!(
                real.join("roomler-agent").join("roomlerd.exe").is_file(),
                "{report:?}"
            );
            assert!(report.removed.is_empty(), "{report:?}");
            assert!(
                report.skipped.iter().any(|s| s.contains("link")),
                "{report:?}"
            );
        }
        // The same folder, reached with no link, is swept.
        let mut report = CleanupReport::default();
        sweep_vacated_dir(root.path(), &real.join("roomler-agent"), &mut report, false);
        assert!(!real.join("roomler-agent").exists(), "{report:?}");
    }

    /// #3702 — the per-user clean-up removes no data tree that is a link,
    /// holds one or is reached through one: such a tree is left in place with
    /// a line that names it, and the other trees are still handled.
    #[cfg(all(target_os = "windows", feature = "system-context"))]
    #[test]
    fn the_per_user_clean_up_leaves_each_tree_with_a_link_in_it_or_on_the_way() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        let elsewhere = root.path().join("elsewhere");
        let kept = [
            elsewhere.join("roomler-agent"),
            elsewhere.join("roomler"),
            elsewhere.join("logs"),
        ];
        for dir in &kept {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("keep.txt"), "x").unwrap();
        }
        let local = profile.join("AppData").join("Local").join("roomler");
        // A tree that holds a link.
        std::fs::create_dir_all(local.join("roomler-agent")).unwrap();
        let inner = local.join("roomler-agent").join("logs");
        junction(&inner, &elsewhere.join("logs"));
        // A tree with no link.
        std::fs::create_dir_all(local.join("roomler").join("logs")).unwrap();
        std::fs::write(local.join("roomler").join("config.toml"), "x").unwrap();
        // A link on the way to both roaming trees.
        let roaming = profile.join("AppData").join("Roaming");
        std::fs::create_dir_all(&roaming).unwrap();
        junction(&roaming.join("roomler"), &elsewhere);

        let plan = plan_for(Some(A_PERSON), &profile);
        assert_eq!(plan.profile.as_deref(), Some(profile.as_path()), "{plan:?}");
        let mut report = CleanupReport::default();
        remove_per_user_trees(plan, &mut report, false);
        for tree in [
            local.join("roomler-agent"),
            roaming.join("roomler").join("roomler-agent"),
            roaming.join("roomler").join("roomler"),
        ] {
            let named = tree.display().to_string();
            assert!(
                report
                    .skipped
                    .iter()
                    .any(|line| line.contains(&named) && line.contains("link")),
                "no line names {named}: {report:?}"
            );
        }
        assert!(report.errors.is_empty(), "{report:?}");
        assert_eq!(report.skipped.len(), 3, "{report:?}");
        assert!(!local.join("roomler").exists(), "{report:?}");
        assert!(std::fs::symlink_metadata(&inner).is_ok(), "{report:?}");
        for dir in &kept {
            assert!(dir.join("keep.txt").is_file(), "{report:?}");
        }

        // A profile that holds none of the trees: each one is named as not
        // present, and nothing is removed.
        let empty = root.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let plan = plan_for(Some(A_PERSON), &empty);
        let mut report = CleanupReport::default();
        remove_per_user_trees(plan, &mut report, false);
        assert!(report.removed.is_empty(), "{report:?}");
        assert_eq!(
            report
                .skipped
                .iter()
                .filter(|line| line.contains("not present"))
                .count(),
            4,
            "{report:?}"
        );
    }

    /// #3702 — on the service account the per-user clean-up names no profile
    /// and removes no data tree, and its one line says so in plain words,
    /// together with the Scheduled Task clean-up that still runs there.
    #[cfg(all(target_os = "windows", feature = "system-context"))]
    #[test]
    fn on_the_service_account_the_per_user_clean_up_names_no_profile_and_removes_nothing() {
        let plan = plan_per_user_removal(
            Some(roomler_node_core::machine_tree::SYSTEM),
            || panic!("a profile was looked up on the service account"),
            |_| panic!("a profile was looked up on the service account"),
        );
        assert!(plan.profile.is_none(), "{plan:?}");
        assert_eq!(plan.left.len(), 1, "{plan:?}");
        for words in [
            "service account",
            "no profile",
            "no user data dir",
            "Scheduled Task",
        ] {
            assert!(plan.left[0].contains(words), "{words}: {plan:?}");
        }
    }

    /// A person's SID, as `win_token::own_user_sid` writes one.
    #[cfg(all(target_os = "windows", feature = "system-context"))]
    const A_PERSON: &str = "S-1-5-21-1-2-3-1001";

    /// The plan when `profile` is the profile of the session's account: its
    /// user is named after the folder, and the system records the folder for
    /// [`A_PERSON`].
    #[cfg(all(target_os = "windows", feature = "system-context"))]
    fn plan_for(own_sid: Option<&str>, profile: &Path) -> PerUserPlan {
        plan_with(
            own_sid,
            SessionAccount::User {
                name: profile.file_name().unwrap().to_string_lossy().into_owned(),
                sid: Some(A_PERSON.into()),
            },
            |sid| (sid == A_PERSON).then(|| profile.to_path_buf()),
        )
    }

    /// The plan with the profile lookup a test names: `session` is the
    /// session's account, and `profile_of` the profile the system records
    /// for a SID.
    #[cfg(all(target_os = "windows", feature = "system-context"))]
    fn plan_with(
        own_sid: Option<&str>,
        session: SessionAccount,
        profile_of: impl FnOnce(&str) -> Option<std::path::PathBuf>,
    ) -> PerUserPlan {
        plan_per_user_removal(own_sid, || session, profile_of)
    }

    /// The four data trees of `profile`, each holding `keep.txt`.
    #[cfg(all(target_os = "windows", feature = "system-context"))]
    fn make_trees(profile: &Path) {
        for tree in per_user_trees(profile) {
            std::fs::create_dir_all(&tree).unwrap();
            std::fs::write(tree.join("keep.txt"), "x").unwrap();
        }
    }

    /// FR-145 T12 — the per-Machine arm removes no `service-logs` through a
    /// link: a junction as the `roomler` folder, then as the segment folder.
    /// What each leads to stays, and the report names the link.
    #[cfg(target_os = "windows")]
    #[test]
    fn the_per_machine_clean_up_removes_no_service_logs_through_a_link() {
        let root = tempfile::tempdir().unwrap();
        // `roomler` is a junction, to a folder that holds both segments.
        let program_data = root.path().join("ProgramData");
        std::fs::create_dir_all(&program_data).unwrap();
        let other = root.path().join("other");
        for segment in SEGMENTS {
            let logs = other.join(segment).join("service-logs");
            std::fs::create_dir_all(&logs).unwrap();
            std::fs::write(logs.join("a.log"), "x").unwrap();
        }
        junction(&program_data.join("roomler"), &other);
        let mut report = CleanupReport::default();
        cleanup_per_machine_service_logs(&program_data, &mut report, false);
        for segment in SEGMENTS {
            assert!(
                other
                    .join(segment)
                    .join("service-logs")
                    .join("a.log")
                    .is_file(),
                "{report:?}"
            );
        }
        assert!(report.removed.is_empty(), "{report:?}");
        assert!(
            report.skipped.iter().any(|s| s.contains("link")),
            "{report:?}"
        );

        // The segment folder is a junction.
        let program_data = root.path().join("ProgramData2");
        std::fs::create_dir_all(program_data.join("roomler")).unwrap();
        let other = root.path().join("other2");
        std::fs::create_dir_all(other.join("service-logs")).unwrap();
        std::fs::write(other.join("service-logs").join("a.log"), "x").unwrap();
        junction(&program_data.join("roomler").join("roomler"), &other);
        let mut report = CleanupReport::default();
        cleanup_per_machine_service_logs(&program_data, &mut report, false);
        assert!(
            other.join("service-logs").join("a.log").is_file(),
            "{report:?}"
        );
        assert!(report.removed.is_empty(), "{report:?}");
        assert!(
            report.skipped.iter().any(|s| s.contains("link")),
            "{report:?}"
        );
    }

    /// FR-145 T13 — with no link on the way, both segments' `service-logs`
    /// are removed, one report line each.
    #[cfg(target_os = "windows")]
    #[test]
    fn the_per_machine_clean_up_still_removes_both_segments_service_logs() {
        let root = tempfile::tempdir().unwrap();
        let program_data = root.path().join("ProgramData");
        for segment in SEGMENTS {
            let logs = program_data
                .join("roomler")
                .join(segment)
                .join("service-logs");
            std::fs::create_dir_all(&logs).unwrap();
            std::fs::write(logs.join("a.log"), "x").unwrap();
        }
        let mut report = CleanupReport::default();
        cleanup_per_machine_service_logs(&program_data, &mut report, false);
        for segment in SEGMENTS {
            let logs = program_data
                .join("roomler")
                .join(segment)
                .join("service-logs");
            assert!(!logs.exists(), "{report:?}");
            assert!(program_data.join("roomler").join(segment).is_dir());
        }
        assert_eq!(
            report
                .removed
                .iter()
                .filter(|line| line.contains("service log dir"))
                .count(),
            2,
            "{report:?}"
        );
        assert!(report.errors.is_empty(), "{report:?}");
    }

    /// FR-145 T14 — the per-user removal acts on the tree it checked: a folder
    /// on the way that becomes a junction after the plan is never followed.
    /// What it leads to stays, and the report names the link.
    #[cfg(all(target_os = "windows", feature = "system-context"))]
    #[test]
    fn the_per_user_removal_acts_on_the_tree_it_checked() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        make_trees(&profile);
        let elsewhere = root.path().join("elsewhere");
        for segment in SEGMENTS {
            std::fs::create_dir_all(elsewhere.join(segment)).unwrap();
            std::fs::write(elsewhere.join(segment).join("keep.txt"), "x").unwrap();
        }
        let plan = plan_for(Some(A_PERSON), &profile);
        // After the plan: `AppData\Local\roomler` moved aside, a junction in
        // its name.
        let local = profile.join("AppData").join("Local").join("roomler");
        std::fs::rename(&local, local.with_file_name("roomler.aside")).unwrap();
        junction(&local, &elsewhere);
        let mut report = CleanupReport::default();
        remove_per_user_trees(plan, &mut report, false);
        for segment in SEGMENTS {
            assert!(
                elsewhere.join(segment).join("keep.txt").is_file(),
                "nothing the junction leads to is removed: {report:?}"
            );
        }
        let named = local.display().to_string();
        assert!(
            report
                .skipped
                .iter()
                .any(|line| line.contains(&named) && line.contains("link")),
            "a line names the link: {report:?}"
        );
        let roaming = profile.join("AppData").join("Roaming").join("roomler");
        for segment in SEGMENTS {
            assert!(!roaming.join(segment).exists(), "{report:?}");
        }
    }

    /// FR-145 T14 (b) — the swap at the act: once the walk holds
    /// `AppData\Local\roomler`, after every check a walk or a re-check by
    /// path could make, that folder is renamed aside and a junction put at
    /// its name (`between_check_and_use`). The tree removed is the held
    /// folder's, below what is now `roomler.aside`; nothing the junction
    /// leads to is removed, and the next walk through the junction is
    /// refused with a line. A removal by path after the walk fails this
    /// case either way: through the junction it deletes what it leads to,
    /// and a re-check that refuses leaves the held tree in place. Its own
    /// process, as the hook is process-wide.
    #[cfg(all(target_os = "windows", feature = "system-context"))]
    #[test]
    fn the_per_user_removal_acts_through_the_folder_it_holds() {
        use roomler_node_core::machine_tree::test_support::between_check_and_use;
        const SWAP: &str = "ROOMLERD_TEST_CLEANUP_SWAP";
        const CHECKED: &str = "per-user swap case: checked";
        if std::env::var_os(SWAP).is_none() {
            let test =
                "install_cleanup::tests::the_per_user_removal_acts_through_the_folder_it_holds";
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([test, "--exact", "--nocapture", "--test-threads=1"])
                .env(SWAP, "1")
                .output()
                .unwrap();
            let said = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(out.status.success(), "{said}");
            assert!(said.contains(CHECKED), "the child ran the case: {said}");
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        make_trees(&profile);
        let elsewhere = root.path().join("elsewhere");
        for segment in SEGMENTS {
            std::fs::create_dir_all(elsewhere.join(segment)).unwrap();
            std::fs::write(elsewhere.join(segment).join("keep.txt"), "x").unwrap();
        }
        let plan = plan_for(Some(A_PERSON), &profile);
        let local = profile.join("AppData").join("Local").join("roomler");
        let aside = local.with_file_name("roomler.aside");
        let (from, to, target) = (local.clone(), aside.clone(), elsewhere.clone());
        between_check_and_use(move || {
            std::fs::rename(&from, &to).expect("the held folder moves aside");
            junction(&from, &target);
        });
        let mut report = CleanupReport::default();
        remove_per_user_trees(plan, &mut report, false);

        assert!(
            aside.is_dir(),
            "the swap ran, once a walk held the folder: {report:?}"
        );
        for segment in SEGMENTS {
            assert!(
                elsewhere.join(segment).join("keep.txt").is_file(),
                "nothing the junction leads to is removed: {report:?}"
            );
        }
        assert!(
            !aside.join(PER_USER_TREES[0].1).exists(),
            "the held folder's tree is the one removed: {report:?}"
        );
        assert!(
            aside.join("roomler").join("keep.txt").is_file(),
            "the next walk meets the junction and removes nothing: {report:?}"
        );
        let named = local.display().to_string();
        assert!(
            report
                .skipped
                .iter()
                .any(|line| line.contains(&named) && line.contains("link")),
            "a line names the link: {report:?}"
        );
        let roaming = profile.join("AppData").join("Roaming").join("roomler");
        for segment in SEGMENTS {
            assert!(!roaming.join(segment).exists(), "{report:?}");
        }
        println!("{CHECKED}");
    }

    /// FR-145 T15 — the per-user clean-up reads the profile the system records
    /// for the session's account: that profile's trees are removed, and a
    /// folder named after the session's user is untouched. With no profile
    /// recorded, nothing is removed and the report says why.
    #[cfg(all(target_os = "windows", feature = "system-context"))]
    #[test]
    fn the_per_user_clean_up_reads_the_profile_the_system_records() {
        let root = tempfile::tempdir().unwrap();
        let recorded = root.path().join("p.000");
        let by_name = root.path().join("p");
        make_trees(&recorded);
        make_trees(&by_name);
        let account = SessionAccount::User {
            name: "p".into(),
            sid: Some(A_PERSON.into()),
        };
        let plan = plan_with(Some(A_PERSON), account.clone(), |sid| {
            (sid == A_PERSON).then(|| recorded.clone())
        });
        let mut report = CleanupReport::default();
        remove_per_user_trees(plan, &mut report, false);
        for tree in per_user_trees(&by_name) {
            assert!(
                tree.join("keep.txt").is_file(),
                "a folder named after the user is untouched: {report:?}"
            );
        }
        for tree in per_user_trees(&recorded) {
            assert!(!tree.exists(), "the recorded profile's trees: {report:?}");
        }

        make_trees(&recorded);
        let plan = plan_with(Some(A_PERSON), account, |_| None);
        let mut report = CleanupReport::default();
        remove_per_user_trees(plan, &mut report, false);
        assert!(report.removed.is_empty(), "{report:?}");
        for profile in [&recorded, &by_name] {
            for tree in per_user_trees(profile) {
                assert!(tree.join("keep.txt").is_file(), "{report:?}");
            }
        }
        assert!(
            report
                .skipped
                .iter()
                .any(|line| line.contains("records no profile")),
            "the report says why: {report:?}"
        );
    }

    /// #3702 — when the step cannot tell which account it runs as, the
    /// per-user clean-up looks up no profile and removes no data tree, and
    /// its one line says why, together with the Scheduled Task clean-up that
    /// still runs.
    #[cfg(all(target_os = "windows", feature = "system-context"))]
    #[test]
    fn with_no_account_named_the_per_user_clean_up_names_no_profile_and_removes_nothing() {
        let plan = plan_per_user_removal(
            None,
            || panic!("a profile was looked up for an account the step cannot name"),
            |_| panic!("a profile was looked up for an account the step cannot name"),
        );
        assert!(plan.profile.is_none(), "{plan:?}");
        assert_eq!(plan.left.len(), 1, "{plan:?}");
        for words in [
            "cannot tell which account",
            "no profile",
            "no user data dir",
            "Scheduled Task",
        ] {
            assert!(plan.left[0].contains(words), "{words}: {plan:?}");
        }
    }

    /// Dry-run `run_cleanup` always records the sweep's verdict —
    /// whichever branch it takes (dir present / absent / own-dir
    /// guard), the report mentions the vacated dir. This is the lock
    /// that the sweep sits BEFORE the same-flavour fast path (the
    /// only placement the MSI CA ever executes).
    #[cfg(target_os = "windows")]
    #[test]
    fn dry_run_cleanup_reports_vacated_dir_sweep_before_fast_path() {
        let report = run_cleanup(TargetFlavour::PerUser, true).unwrap();
        assert!(
            report
                .removed
                .iter()
                .chain(report.skipped.iter())
                .any(|s| s.contains("vacated")),
            "sweep verdict missing from report: {}",
            report.summary()
        );
        // ... and the fast path still fired after it (this test
        // binary never runs from Program Files → current == PerUser
        // == target).
        assert!(
            report
                .skipped
                .iter()
                .any(|s| s.contains("same-flavour install")),
            "fast path missing from report: {}",
            report.summary()
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn same_dir_is_case_insensitive_and_rejects_distinct_dirs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let a = tmp.path().join("Alpha");
        let b = tmp.path().join("beta");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        assert!(same_dir(&a, &a));
        // Windows path semantics: differing case, same dir.
        let a_upper = tmp.path().join("ALPHA");
        assert!(same_dir(&a, &a_upper));
        assert!(!same_dir(&a, &b));
    }

    // ── #3801 the per-user arm's Scheduled Tasks ─────────────────

    /// Two made-up accounts of one machine, each with a per-user install, and
    /// the operator who runs the clean-up for the first.
    #[cfg(target_os = "windows")]
    const ACCOUNT_A: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";
    #[cfg(target_os = "windows")]
    const ACCOUNT_B: &str = "S-1-5-21-1111111111-2222222222-3333333333-1002";
    #[cfg(target_os = "windows")]
    const OPERATOR: &str = "S-1-5-21-1111111111-2222222222-3333333333-500";

    /// #3801 — the per-user arm, run for A on a machine where B has a per-user
    /// install too, removes A's tasks and leaves B's, in each state two such
    /// installs can be in: both on their own task, A on its own and B still on
    /// the shared name an older release registered, and the other way round.
    /// Its report says what it left, and names neither account.
    #[cfg(target_os = "windows")]
    #[test]
    fn the_per_user_clean_up_removes_its_accounts_task_and_leaves_anothers() {
        use crate::service::user_task::{RETIRED, SHARED, fake::Library as Fake};
        let a = format!("Roomler-{ACCOUNT_A}");
        let b = format!("Roomler-{ACCOUNT_B}");
        let states = [
            (
                "both on their own task",
                vec![(a.as_str(), ACCOUNT_A), (b.as_str(), ACCOUNT_B)],
            ),
            (
                "B on the shared name",
                vec![(a.as_str(), ACCOUNT_A), (SHARED, ACCOUNT_B)],
            ),
            (
                "A on the shared names",
                vec![
                    (SHARED, ACCOUNT_A),
                    (RETIRED, ACCOUNT_A),
                    (b.as_str(), ACCOUNT_B),
                ],
            ),
        ];
        for (state, tasks) in &states {
            for dry_run in [false, true] {
                let mut lib = Fake::new(OPERATOR, true);
                for (name, sid) in tasks {
                    lib = lib.with(name, Some(*sid));
                }
                let mut report = CleanupReport::default();
                clean_up_user_tasks(&mut lib, Ok(ACCOUNT_A), &mut report, dry_run);
                let said = report.summary();
                for (name, sid) in tasks {
                    let mine = *sid == ACCOUNT_A;
                    assert_eq!(
                        lib.owner(name).is_some(),
                        !mine || dry_run,
                        "{state}, dry run {dry_run}: {name}: {said}"
                    );
                    if !mine {
                        assert!(!lib.asked_for(name), "{state}: {:?}", lib.asked);
                    }
                }
                let own = tasks.iter().filter(|(_, sid)| *sid == ACCOUNT_A).count();
                assert_eq!(report.removed.len(), own, "{state}: {said}");
                assert_eq!(
                    report
                        .skipped
                        .iter()
                        .filter(|line| line.contains("left in place: it runs as another account"))
                        .count(),
                    1,
                    "{state}: {said}"
                );
                assert!(report.errors.is_empty(), "{state}: {said}");
                for sid in [ACCOUNT_A, ACCOUNT_B] {
                    assert!(!said.contains(sid), "the report names an account: {said}");
                }
            }
        }
    }

    /// #3801 — a task under a shared name whose account the clean-up cannot
    /// read is left in place, and the report says why: another account's,
    /// listed to the person running it with its definition refused, or one
    /// whose definition names no account.
    #[cfg(target_os = "windows")]
    #[test]
    fn the_per_user_clean_up_leaves_a_task_whose_account_it_cannot_read() {
        use crate::service::user_task::{SHARED, fake::Library as Fake};
        let cases = [
            (
                "refused",
                Fake::new(ACCOUNT_A, false).with_seen_by_all(SHARED, Some(ACCOUNT_B)),
            ),
            ("unparsable", Fake::new(OPERATOR, true).with(SHARED, None)),
        ];
        for (case, mut lib) in cases {
            let mut report = CleanupReport::default();
            clean_up_user_tasks(&mut lib, Ok(ACCOUNT_A), &mut report, false);
            let said = report.summary();
            assert!(lib.owner(SHARED).is_some(), "{case}: {said}");
            assert!(!lib.asked_for(SHARED), "{case}: {:?}", lib.asked);
            assert!(
                report.skipped.iter().any(|line| line
                    .starts_with("Scheduled Task Roomler left in place")
                    && line.contains("cannot tell which account it runs as")),
                "{case}: {said}"
            );
        }
    }

    /// #3801 — with no account to clean (nobody signed in to the step's
    /// session, or an account it cannot tell), the per-user arm removes no
    /// Scheduled Task, and each one it leaves has a line that says why.
    #[cfg(target_os = "windows")]
    #[test]
    fn with_no_account_to_clean_the_per_user_clean_up_removes_no_task() {
        use crate::service::user_task::{RETIRED, SHARED, fake::Library as Fake};
        let why = "nobody is signed in to this step's session";
        let a = format!("Roomler-{ACCOUNT_A}");
        let mut lib = Fake::new(OPERATOR, true)
            .with(SHARED, Some(ACCOUNT_A))
            .with(RETIRED, Some(ACCOUNT_B))
            .with(&a, Some(ACCOUNT_A));
        let mut report = CleanupReport::default();
        clean_up_user_tasks(&mut lib, Err(why), &mut report, false);
        let said = report.summary();
        assert!(lib.asked.is_empty(), "{:?}", lib.asked);
        assert!(report.removed.is_empty(), "{said}");
        assert_eq!(
            report
                .skipped
                .iter()
                .filter(|line| line.contains("left in place") && line.contains(why))
                .count(),
            3,
            "{said}"
        );
        assert!(!said.contains(ACCOUNT_A), "{said}");
    }
}
// RETIRED-NAME-ANCHOR-END

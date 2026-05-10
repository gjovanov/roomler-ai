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
//! - Service log dir `%PROGRAMDATA%\roomler\roomler-agent\service-logs\`
//!   (best-effort; may be ACL-locked).
//!
//! When installing **perMachine**, the helper cleans **perUser**
//! leftovers:
//! - Scheduled Task `RoomlerAgent` (`schtasks /Delete /TN RoomlerAgent
//!   /F`). Runs as SYSTEM, can see + delete the user-scope task.
//! - Active-session user's data dirs at `%LOCALAPPDATA%\roomler\
//!   roomler-agent\` and `%APPDATA%\roomler\roomler-agent\` (resolved
//!   via `system_context::user_profile::active_user_profile_root()`
//!   because SYSTEM context can't see `%LOCALAPPDATA%` of the
//!   interactive user directly).
//! - `needs-attention.txt` sentinel in the user's config dir.
//!
//! ## Same-flavour fast path
//!
//! When the new install matches the existing install's flavour, this
//! helper exits 0 immediately. Same-flavour upgrades go through WiX
//! MajorUpgrade and don't need any cross-flavour scrubbing.
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
use std::path::PathBuf;

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

// ──────────────────────────────────────────────────────────────────
// perMachine → cleanup perUser artifacts (we are installing perMachine
// over a prior perUser install)
// ──────────────────────────────────────────────────────────────────
#[cfg(target_os = "windows")]
fn cleanup_per_user_artifacts(report: &mut CleanupReport, dry_run: bool) {
    // 1. Scheduled Task `RoomlerAgent` — registered by the perUser MSI's
    //    `RegisterAutostart` custom action. perMachine MSI's CA runs as
    //    SYSTEM so it can see + delete the task without impersonation.
    if scheduled_task_exists("RoomlerAgent") {
        if dry_run {
            report
                .removed
                .push("[dry-run] schtasks /Delete /TN RoomlerAgent /F".to_string());
        } else {
            match delete_scheduled_task("RoomlerAgent") {
                Ok(()) => report
                    .removed
                    .push("Scheduled Task RoomlerAgent".to_string()),
                Err(e) => report.errors.push(format!("schtasks /Delete: {e}")),
            }
        }
    } else {
        report
            .skipped
            .push("Scheduled Task RoomlerAgent not present".to_string());
    }

    // 2. Active-session user's data dirs. SYSTEM context can't see the
    //    interactive user's %LOCALAPPDATA% / %APPDATA% directly; resolve
    //    via the WTSQueryUserToken-backed `active_user_profile_root()`
    //    (reuses the M3 A1 SystemContext plumbing).
    #[cfg(feature = "system-context")]
    {
        if let Some(profile_root) = crate::system_context::user_profile::active_user_profile_root()
        {
            let candidates = [
                profile_root
                    .join("AppData")
                    .join("Local")
                    .join("roomler")
                    .join("roomler-agent"),
                profile_root
                    .join("AppData")
                    .join("Roaming")
                    .join("roomler")
                    .join("roomler-agent"),
            ];
            for path in candidates {
                if path.exists() {
                    if dry_run {
                        report
                            .removed
                            .push(format!("[dry-run] rmdir /s {}", path.display()));
                    } else {
                        match std::fs::remove_dir_all(&path) {
                            Ok(()) => report
                                .removed
                                .push(format!("user data dir {}", path.display())),
                            Err(e) => {
                                report
                                    .errors
                                    .push(format!("rmdir {}: {}", path.display(), e))
                            }
                        }
                    }
                } else {
                    report
                        .skipped
                        .push(format!("user data dir {} not present", path.display()));
                }
            }
        } else {
            // No active interactive session (headless install / no
            // logged-in user). Cleanup is a no-op; the orphan data
            // remains until a user logs in and runs `cleanup-legacy-
            // install` manually OR until the user's own uninstall.
            report
                .skipped
                .push("no active interactive session — user data dirs not reachable".to_string());
        }
    }

    // Without the system-context feature compiled in, the
    // active_user_profile_root helper isn't available; the cleanup
    // becomes a perUser-self-deletes-its-own-dirs operation. Since
    // perMachine MSI normally has the feature on, this branch only
    // matters for the default build used in headless CI tests.
    #[cfg(not(feature = "system-context"))]
    {
        report
            .skipped
            .push("system-context feature off — user data dir reach-through skipped".to_string());
    }
}

// ──────────────────────────────────────────────────────────────────
// perUser → cleanup perMachine artifacts (we are installing perUser
// over a prior perMachine install)
// ──────────────────────────────────────────────────────────────────
#[cfg(target_os = "windows")]
fn cleanup_per_machine_artifacts(report: &mut CleanupReport, dry_run: bool) {
    // 1. SCM service `RoomlerAgentService`. The perUser MSI's CA runs
    //    Impersonated (user token). Service control requires admin —
    //    so this is BEST-EFFORT. If the user isn't admin, sc fails and
    //    we surface it as an error (the operator must manually
    //    `sc delete RoomlerAgentService` from elevated PS).
    if service_exists("RoomlerAgentService") {
        if dry_run {
            report
                .removed
                .push("[dry-run] sc stop + sc delete RoomlerAgentService".to_string());
        } else {
            // stop is best-effort (service may already be stopped).
            let _ = run_quiet("sc", &["stop", "RoomlerAgentService"]);
            match run_quiet("sc", &["delete", "RoomlerAgentService"]) {
                Ok(()) => report
                    .removed
                    .push("SCM service RoomlerAgentService".to_string()),
                Err(e) => report
                    .errors
                    .push(format!("sc delete RoomlerAgentService: {e}")),
            }
        }
    } else {
        report
            .skipped
            .push("SCM service RoomlerAgentService not present".to_string());
    }

    // 2. Service log dir at %PROGRAMDATA%\roomler\roomler-agent\
    //    service-logs\. ACL'd to SYSTEM at create time; the perUser
    //    MSI's user-token CA may not be able to delete it.
    if let Ok(program_data) = std::env::var("PROGRAMDATA") {
        let path = PathBuf::from(program_data)
            .join("roomler")
            .join("roomler-agent")
            .join("service-logs");
        if path.exists() {
            if dry_run {
                report
                    .removed
                    .push(format!("[dry-run] rmdir /s {}", path.display()));
            } else {
                match std::fs::remove_dir_all(&path) {
                    Ok(()) => report
                        .removed
                        .push(format!("service log dir {}", path.display())),
                    Err(e) => report
                        .errors
                        .push(format!("rmdir {}: {}", path.display(), e)),
                }
            }
        } else {
            report
                .skipped
                .push(format!("service log dir {} not present", path.display()));
        }
    }
}

#[cfg(target_os = "windows")]
fn scheduled_task_exists(name: &str) -> bool {
    // schtasks returns 0 when the task exists, non-zero otherwise.
    // We don't need the output — exit code suffices.
    std::process::Command::new("schtasks")
        .args(["/Query", "/TN", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
fn delete_scheduled_task(name: &str) -> Result<()> {
    let status = std::process::Command::new("schtasks")
        .args(["/Delete", "/TN", name, "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if !status.success() {
        anyhow::bail!("schtasks /Delete exited {:?}", status.code());
    }
    Ok(())
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
}

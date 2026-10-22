// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3b-1b-2 — where the device's owner turned on `hive_egress`, a
//! session runs in Claude Code's sandbox or not at all (decision 28): its
//! commands reach only the hosts the vault allows, through the session's
//! egress proxy ([`super::egress`]).
//!
//! The settings make a host where the sandbox cannot run refuse to start
//! (`failIfUnavailable`), and Claude Code would exit. This gate refuses the
//! start first, in words the person who started it can act on
//! (`sandbox_unavailable`):
//!
//! | Refused where | Because |
//! |---|---|
//! | the device takes no leases | no vault decides the hosts a sandboxed session reaches |
//! | Linux without `bwrap` or `socat` | the sandbox's isolation and its relay |
//! | a harness older than [`MIN_HARNESS`], or one that does not say | before it, a repository's own `.claude/settings.json` could add `excludedCommands`, which run outside the sandbox and past the proxy |
//!
//! ⚠️ The harness is asked its version AS THE SESSION'S ACCOUNT, never as the
//! daemon: it can be the account's own (`~/.local/bin/claude`), which the
//! daemon must never run as itself. Its answer is trusted as the harness is
//! trusted to sandbox at all: the sandbox holds the model's commands, not the
//! account's owner.
//!
//! P3b-5-1 — on Windows no sandbox runs and none is asked for: the session's
//! egress proxy is advisory there (decision 30), named in its settings'
//! environment. The gate refuses only a device that takes no leases
//! ([`decide_advisory`]); no harness version is asked, since none of the
//! settings its floor guards is written.

#[cfg(unix)]
use std::collections::HashMap;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::{LazyLock, Mutex};
#[cfg(unix)]
use std::time::{Duration, SystemTime};

use roomler_ai_remote_control::hive::HiveRefusal;

/// The oldest Claude Code whose sandbox, made admin-required by
/// `allowUnsandboxedCommands: false`, ignores what a repository's own
/// settings would add to loosen it (`excludedCommands`, `allowWrite`, its
/// proxy ports). Before it, a repository's `excludedCommands` merged in.
pub const MIN_HARNESS: (u32, u32, u32) = (2, 1, 285);

/// How long the harness has to say its version.
#[cfg(unix)]
const VERSION_TIMEOUT: Duration = Duration::from_secs(20);

/// The tools Claude Code's sandbox needs on Linux: its isolation, and the
/// relay that carries a command's connections to the proxy.
#[cfg(any(target_os = "linux", all(test, unix)))]
pub const LINUX_TOOLS: &[&str] = &["bwrap", "socat"];

/// `X.Y.Z (Claude Code)`, or a bare `X.Y.Z`: the first word that reads as
/// three numbers.
pub fn parse_version(out: &str) -> Option<(u32, u32, u32)> {
    out.split_whitespace().find_map(|word| {
        let mut parts = word.split('.');
        let (Some(a), Some(b), Some(c), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        Some((a.parse().ok()?, b.parse().ok()?, c.parse().ok()?))
    })
}

/// The tools of [`LINUX_TOOLS`] found in none of `dirs`.
#[cfg(any(target_os = "linux", all(test, unix)))]
pub fn missing_tools(dirs: &[PathBuf]) -> Vec<&'static str> {
    use std::os::unix::fs::PermissionsExt;
    LINUX_TOOLS
        .iter()
        .copied()
        .filter(|tool| {
            !dirs.iter().any(|d| {
                std::fs::metadata(d.join(tool))
                    .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false)
            })
        })
        .collect()
}

/// The gate's word, from what was found: `Ok` lets the start through.
pub fn decide(
    vault: bool,
    missing: &[&str],
    version: Result<(u32, u32, u32), String>,
) -> Result<(), (HiveRefusal, String)> {
    let refuse = |why: String| Err((HiveRefusal::SandboxUnavailable, why));
    if !vault {
        return refuse(
            "hive_egress runs every session here sandboxed, and the organization's vault decides \
             each host it reaches; this device takes no leases (vault_enabled), so nothing would \
             be reached"
                .into(),
        );
    }
    if !missing.is_empty() {
        return refuse(format!(
            "Claude Code's sandbox needs bubblewrap and socat on this device, and {} is not \
             installed",
            missing.join(" and ")
        ));
    }
    match version {
        Ok(v) if v >= MIN_HARNESS => Ok(()),
        Ok((a, b, c)) => {
            let (x, y, z) = MIN_HARNESS;
            refuse(format!(
                "Claude Code {a}.{b}.{c} on this device is older than {x}.{y}.{z}, the first \
                 whose sandbox a repository's own settings cannot loosen"
            ))
        }
        Err(e) => refuse(format!(
            "could not tell which Claude Code this device runs, so not whether its sandbox holds: \
             {e}"
        )),
    }
}

/// P3b-5-1 — the gate's word where no sandbox runs (Windows): the session's
/// egress proxy is advisory there, so a vault to decide its hosts is all a
/// start needs.
pub fn decide_advisory(vault: bool) -> Result<(), (HiveRefusal, String)> {
    if vault {
        return Ok(());
    }
    Err((
        HiveRefusal::SandboxUnavailable,
        "hive_egress sends every session's traffic here through its egress proxy, and the \
         organization's vault decides each host it reaches; this device takes no leases \
         (vault_enabled), so nothing would be reached"
            .into(),
    ))
}

/// Which file a version was read from: its path, and its size and change
/// time, so an update asks again.
#[cfg(unix)]
type Seen = (PathBuf, u64, Option<SystemTime>);

/// A harness's version, as [`parse_version`] reads it.
#[cfg(unix)]
type Version = (u32, u32, u32);

/// Each harness file's version, while the file is the same.
#[cfg(unix)]
type Versions = Mutex<HashMap<Seen, Version>>;

#[cfg(unix)]
static VERSIONS: LazyLock<Versions> = LazyLock::new(Mutex::default);

#[cfg(unix)]
fn seen(harness: &Path) -> Option<Seen> {
    let m = std::fs::metadata(harness).ok()?;
    Some((harness.to_path_buf(), m.len(), m.modified().ok()))
}

/// The gate for a start as `account`, on a device whose owner turned on
/// `hive_egress`. `as_account` is false only for a test's launcher, which
/// runs the harness as the daemon.
pub async fn gate(
    account: &str,
    harness_cfg: Option<&Path>,
    as_account: bool,
) -> Result<(), (HiveRefusal, String)> {
    #[cfg(windows)]
    {
        let _ = (account, harness_cfg, as_account);
        decide_advisory(super::toolbelt::daemon_vault().is_some())
    }
    #[cfg(unix)]
    {
        // A missing account or harness is refused as missing, by the launch.
        let Ok(home) = crate::exec::account_home(account) else {
            return Ok(());
        };
        let Some(harness) = super::supervisor::resolve_harness_for(harness_cfg, &home) else {
            return Ok(());
        };
        // Each refusal before anything runs: no harness is asked its version
        // where the start is refused anyway.
        if super::toolbelt::daemon_vault().is_none() {
            return decide(false, &[], Ok(MIN_HARNESS));
        }
        #[cfg(target_os = "linux")]
        let missing = missing_tools(&super::supervisor::harness_dirs(&home));
        #[cfg(not(target_os = "linux"))]
        let missing: Vec<&str> = Vec::new();
        if !missing.is_empty() {
            return decide(true, &missing, Ok(MIN_HARNESS));
        }
        decide(
            true,
            &[],
            version_of(&harness, account, &home, as_account).await,
        )
    }
}

/// The version `harness` says it is, asked as `account` (or as the daemon,
/// for a test's launcher), and kept for as long as the file is the same.
#[cfg(unix)]
async fn version_of(
    harness: &Path,
    account: &str,
    home: &Path,
    as_account: bool,
) -> Result<(u32, u32, u32), String> {
    use std::process::Stdio;
    let key = seen(harness);
    if let Some(k) = &key
        && let Some(v) = VERSIONS.lock().unwrap_or_else(|e| e.into_inner()).get(k)
    {
        return Ok(*v);
    }
    let mut cmd = tokio::process::Command::new(harness);
    cmd.arg("--version")
        .env_clear()
        .envs(super::supervisor::harness_env(home, account))
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if as_account {
        crate::exec::apply_session_run_as(&mut cmd, account)?;
    }
    let out = tokio::time::timeout(VERSION_TIMEOUT, cmd.output())
        .await
        .map_err(|_| {
            format!(
                "{} --version did not answer within {} s",
                harness.display(),
                VERSION_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| format!("running {} --version: {e}", harness.display()))?;
    let said = String::from_utf8_lossy(&out.stdout);
    let version = parse_version(&said).ok_or_else(|| {
        let said: String = said.trim().chars().take(120).collect();
        format!(
            "{} --version said {said:?} (exit {:?})",
            harness.display(),
            out.status.code()
        )
    })?;
    if let Some(k) = key {
        VERSIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(k, version);
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `claude --version` prints, and a bare number, read alike;
    /// nothing else passes for a version.
    #[test]
    fn a_version_is_read_from_what_the_harness_says() {
        assert_eq!(parse_version("2.1.285 (Claude Code)\n"), Some((2, 1, 285)));
        assert_eq!(parse_version("2.10.3"), Some((2, 10, 3)));
        for said in ["", "Claude Code", "2.1", "2.1.x (Claude Code)", "2.1.285.1"] {
            assert_eq!(parse_version(said), None, "{said:?}");
        }
    }

    /// The gate lets a start through only with a vault, every tool, and a
    /// harness at least [`MIN_HARNESS`]; each refusal is
    /// `sandbox_unavailable`, saying what to fix.
    #[test]
    fn a_start_is_let_through_only_where_the_sandbox_holds() {
        assert_eq!(decide(true, &[], Ok(MIN_HARNESS)), Ok(()));
        assert_eq!(decide(true, &[], Ok((2, 2, 0))), Ok(()));
        assert_eq!(decide(true, &[], Ok((3, 0, 0))), Ok(()));
        for (vault, missing, version, says) in [
            (false, vec![], Ok(MIN_HARNESS), "takes no leases"),
            (
                true,
                vec!["socat"],
                Ok(MIN_HARNESS),
                "socat is not installed",
            ),
            (
                true,
                vec![],
                Ok((2, 1, 284)),
                "2.1.284 on this device is older than 2.1.285",
            ),
            (true, vec![], Ok((1, 9, 999)), "older than"),
            (true, vec![], Err("no answer".to_string()), "could not tell"),
        ] {
            let refused = decide(vault, &missing, version).expect_err(says);
            assert_eq!(refused.0, HiveRefusal::SandboxUnavailable, "{says}");
            assert!(refused.1.contains(says), "{says}: {}", refused.1);
        }
    }

    /// P3b-5-1 — where no sandbox runs (Windows), a start needs a vault to
    /// decide its hosts, and nothing more: no tool, no harness version.
    #[test]
    fn where_no_sandbox_runs_a_start_needs_the_vault_alone() {
        assert_eq!(decide_advisory(true), Ok(()));
        let refused = decide_advisory(false).unwrap_err();
        assert_eq!(refused.0, HiveRefusal::SandboxUnavailable);
        assert!(refused.1.contains("takes no leases"), "{}", refused.1);
        assert!(
            !refused.1.contains("sandboxed"),
            "no sandbox is said where none runs: {}",
            refused.1
        );
    }

    /// The tools are looked for in the directories the harness is given, an
    /// executable file in any one of them enough.
    #[cfg(unix)]
    #[test]
    fn the_tools_are_found_where_the_harness_looks() {
        use std::os::unix::fs::PermissionsExt;
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let dirs = vec![a.path().to_path_buf(), b.path().to_path_buf()];
        assert_eq!(missing_tools(&dirs), ["bwrap", "socat"]);
        let bwrap = a.path().join("bwrap");
        std::fs::write(&bwrap, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bwrap, std::fs::Permissions::from_mode(0o755)).unwrap();
        let socat = b.path().join("socat");
        std::fs::write(&socat, "not executable").unwrap();
        std::fs::set_permissions(&socat, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            missing_tools(&dirs),
            ["socat"],
            "a file that will not run is no tool"
        );
        std::fs::set_permissions(&socat, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(missing_tools(&dirs).is_empty());
    }

    /// The harness is asked its version, and asked again once its file is
    /// another: an update is never judged by the version it replaced. One
    /// that says no version is refused, saying what it said.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_harness_is_asked_its_version_again_once_it_changed() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let harness = home.path().join("claude");
        // Each version is copied in by a child, never written by this
        // process: a process another test forks meanwhile would keep this
        // one's handle open for writing, and running the harness would find
        // it busy ("Text file busy").
        let says = |said: &str| {
            let next = home.path().join("next");
            std::fs::write(&next, format!("#!/bin/sh\necho '{said}'\n")).unwrap();
            let copied = std::process::Command::new("cp")
                .arg(&next)
                .arg(&harness)
                .status()
                .unwrap();
            assert!(copied.success(), "cp: {copied}");
            std::fs::set_permissions(&harness, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        says("2.1.284 (Claude Code)");
        assert_eq!(
            version_of(&harness, "dev", home.path(), false).await,
            Ok((2, 1, 284))
        );
        says("2.1.1000 (Claude Code)");
        assert_eq!(
            version_of(&harness, "dev", home.path(), false).await,
            Ok((2, 1, 1000)),
            "the updated harness is asked again"
        );
        says("nothing to say");
        let refused = version_of(&harness, "dev", home.path(), false)
            .await
            .expect_err("no version");
        assert!(refused.contains("said \"nothing to say\""), "{refused}");
    }
}

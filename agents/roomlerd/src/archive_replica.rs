// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P2h-1 — `roomlerd enroll --archive-replica`: an archive replica of
//! the organization, as the archive image enrolls one (spec §3b, "The image").
//!
//! An archive replica is an always-on device that holds copies of the org's
//! sessions for their retention and serves a session's room when no other
//! member is online. It runs no session itself. Its whole posture is written
//! WITH the enrolled config, as `--overlay` writes `overlay_enabled`, so its
//! first start is already one; a script appending keys to the file after
//! enrollment would land them under whatever `[table]` came last.
//!
//! | Key | Value | Why |
//! |---|---|---|
//! | `hive_replica` | on | it holds copies of the org's sessions |
//! | `hive_archive` | on | it offers itself as an archive replica; an `ADMINISTRATOR` still designates it before it joins anything |
//! | `hive_enabled` | off | it runs no session and calls no model |
//! | `exec_enabled`, `ssh_enabled` | off | it reaches the org's network for no one |
//! | `forward_acl.enabled` | off | no tunnel exit: an empty allowlist would otherwise trust the server |
//! | `auto_update` | off | it updates by a new container on the same volume, never by a package installed inside one |
//!
//! ⚠️ Only into a FRESH config, and never an ephemeral one: the volume IS the
//! device (its config holds the agent token and the WireGuard key, its store
//! every session placed there), and FR-51 hard-deletes an ephemeral device
//! once it falls silent. Both are refused before the single-use token is
//! spent.

use anyhow::{Result, bail};

use crate::config::AgentConfig;

/// Whether `enroll --archive-replica` may go ahead: checked before the
/// enrollment token is spent, so a refusal leaves no device row behind.
/// `existing` = a config is already at the target path.
pub fn preflight(existing: bool, ephemeral: bool) -> Result<()> {
    if !cfg!(hive_host) {
        bail!(
            "--archive-replica: this build holds no copies of agent sessions (it was \
             built without the `hive` feature, or for a system that runs none)"
        );
    }
    if ephemeral {
        bail!(
            "--archive-replica cannot be --ephemeral: an ephemeral device is deleted \
             once it falls silent, and an archive replica keeps the org's sessions \
             for their retention. Use a standard enrollment token."
        );
    }
    if existing {
        bail!(
            "--archive-replica needs a fresh config: the volume is the device, and an \
             existing enrollment would be turned into an archive replica in place. \
             Point --config at an empty volume."
        );
    }
    Ok(())
}

/// The archive replica's posture, onto the enrolled config before its first
/// save: the table in the module's docs.
pub fn apply(cfg: &mut AgentConfig) {
    cfg.hive_replica = true;
    cfg.hive_archive = true;
    cfg.hive_enabled = false;
    cfg.exec_enabled = false;
    cfg.ssh_enabled = false;
    cfg.forward_acl.enabled = false;
    cfg.auto_update = Some(false);
    tracing::info!(
        "archive replica: holds the org's copies; runs no session, takes no command \
         or SSH session, is no tunnel exit, and never updates itself"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An archive replica holds copies and offers itself, and runs nothing,
    /// reaches nothing and updates by a new container: whatever the fresh
    /// config said.
    #[test]
    fn the_archive_posture_is_written_with_the_enrollment() {
        let mut cfg = crate::config::test_fixture();
        cfg.hive_enabled = true;
        cfg.exec_enabled = true;
        cfg.ssh_enabled = true;
        assert!(
            cfg.forward_acl.enabled,
            "an empty allowlist trusts the server by default"
        );
        apply(&mut cfg);
        assert!(cfg.hive_replica && cfg.hive_archive);
        assert!(!cfg.hive_enabled, "it runs no session");
        assert!(
            !cfg.exec_enabled && !cfg.ssh_enabled,
            "it reaches nothing for anyone"
        );
        assert!(!cfg.forward_acl.enabled, "no tunnel exit");
        assert_eq!(cfg.auto_update, Some(false), "no self-update");
    }

    /// Refused before the token is spent: an ephemeral enrollment, and an
    /// existing config. A fresh, standard one goes ahead where the build
    /// holds copies.
    #[test]
    fn only_a_fresh_standard_enrollment_becomes_an_archive_replica() {
        let ephemeral = preflight(false, true).unwrap_err().to_string();
        let existing = preflight(true, false).unwrap_err().to_string();
        if cfg!(hive_host) {
            assert!(ephemeral.contains("ephemeral"), "{ephemeral}");
            assert!(existing.contains("fresh config"), "{existing}");
            assert!(preflight(false, false).is_ok());
        } else {
            assert!(ephemeral.contains("hive"), "{ephemeral}");
            assert!(preflight(false, false).is_err());
        }
    }
}

// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #2338 — the wizard's enroll step, on every platform: the fresh enrollment
//! is **folded into** the config already at its path, exactly as
//! `roomlerd enroll` does, instead of replacing it.
//!
//! Before this the wizard built a fresh `AgentConfig` from the enroll
//! response and saved it, so a reinstall wiped everything operator-owned:
//! `overlay_enabled` (the node dropped out of the mesh), the WireGuard key
//! (a rotation), `tunnel_routes`, forward ACLs, the SSH policy, the consent
//! and lock settings, secondary orgs. rc.204's fix lived in the CLI
//! (`enrollment::preserve_operator_config`, reached through
//! `apply_enrollment`), which the old installer shelled out to and the
//! unified wizard, enrolling in-process, bypassed.
//!
//! Three rules, each a test below:
//!
//! 1. The existing config is read first and its `machine_id` reused, so the
//!    server recognises the machine; the fresh identity is folded in through
//!    [`roomlerd::enrollment::apply_enrollment`] — a refresh of the same org
//!    keeps every operator-owned key, another org is appended, no config is
//!    written fresh.
//! 2. A fresh install's defaults (FR-99's `lock_on_end = auto` for a Windows
//!    service install; the role's overlay default) apply to a **fresh**
//!    config only. A value the device already has is never overridden by a
//!    default (the FR-99 P3c note on #2338).
//! 3. The Advanced options the person **touched** apply to a fresh and to a
//!    refreshed config alike — the box they ticked is what lands; the ones
//!    they never touched are written only on a fresh config. On an appended
//!    or refreshed *secondary* org nothing is applied: those keys are the
//!    primary's.
//!
//! The Linux **system** scope cannot read the root daemon's config to merge
//! it here (it is root's, 0600); its merge runs inside the privileged
//! `install.sh` half through `roomlerd adopt-enrollment`, which applies the
//! same rules with the same function — [`crate::orchestrator_unix`].
//!
//! Two more rules, shared with `adopt-enrollment` through the agent's core:
//!
//! 4. A config that is there and does not load is never read as "absent"
//!    ([`read_existing`]): its last good copy (`.prev`) is folded into when
//!    that one loads, the damaged file is kept aside as `.unreadable`, and the
//!    person is told which happened (F4).
//! 5. A refresh of the same org that the server answered with ANOTHER device
//!    row does not keep the WireGuard key the previous row still claims
//!    ([`Enrolled::previous_row`], F3).

use std::path::Path;

use roomlerd::config::{AgentConfig, MergeBase};
use roomlerd::enrollment::{EnrollInputs, EnrollOutcome, InstallInfo, PreviousRow};

use crate::orchestrator_agent::AdvancedOptions;

/// A folded enrollment: the config to save, and what the fold did.
pub struct Enrolled {
    pub cfg: AgentConfig,
    pub outcome: EnrollOutcome,
    /// The server answered with another device row than the config held
    /// (its WireGuard key was retired from `cfg`); the caller tells the
    /// person ([`roomlerd::enrollment::describe_previous_row`]).
    pub previous_row: Option<PreviousRow>,
}

/// What a fold found at the config path.
pub struct Existing {
    /// The config to fold into: the one at the path, or its last good copy.
    pub cfg: Option<AgentConfig>,
    /// For the person, when the file at the path did not load.
    pub warning: Option<String>,
}

/// F4 — read the config at `path` for a fold: a file there that does not
/// load is recovered from its last good copy when that one loads, kept aside
/// either way, and said — never silently "absent", which would write a fresh
/// config over the device's settings ([`roomlerd::config::read_merge_base`]).
pub fn read_existing(path: &Path) -> Existing {
    match roomlerd::config::read_merge_base(&path.to_path_buf()) {
        MergeBase::Absent => Existing {
            cfg: None,
            warning: None,
        },
        MergeBase::Loaded(cfg) => Existing {
            cfg: Some(cfg),
            warning: None,
        },
        MergeBase::Unreadable { reason, recovered } => {
            let kept = roomlerd::config::keep_unreadable(path).map_err(|e| e.to_string());
            let warning =
                roomlerd::config::describe_unreadable(path, &reason, recovered.is_some(), &kept);
            tracing::warn!(path = %path.display(), %reason, recovered = recovered.is_some(), "the existing config does not load");
            Existing {
                cfg: recovered,
                warning: Some(warning),
            }
        }
    }
}

/// Enroll `device_name` against `server` with `token`, for the config at
/// `config_path`, and fold the answer into `existing` (the config read from
/// that path, if any). The machine id is the existing config's, else derived
/// from the path, as `roomlerd enroll` does.
pub async fn enroll_folded(
    config_path: &Path,
    existing: Option<AgentConfig>,
    server: &str,
    token: &str,
    device_name: &str,
    install: InstallInfo,
) -> anyhow::Result<Enrolled> {
    let machine_id = match &existing {
        Some(cfg) => {
            tracing::info!(machine_id = %cfg.machine_id, "existing config found — reusing its machine fingerprint");
            cfg.machine_id.clone()
        }
        None => roomlerd::machine::derive_machine_id(config_path),
    };
    let fresh = roomlerd::enrollment::enroll(EnrollInputs {
        server_url: server,
        enrollment_token: token,
        machine_id: &machine_id,
        machine_name: device_name,
        install: Some(install),
    })
    .await?;
    fold(existing, fresh)
}

/// The fold itself, without the network: `apply_enrollment`, then the
/// previous row's key retired when the server made another row (F3).
fn fold(existing: Option<AgentConfig>, fresh: AgentConfig) -> anyhow::Result<Enrolled> {
    let previous = existing.clone();
    let (mut cfg, outcome) = roomlerd::enrollment::apply_enrollment(existing, fresh, None, false)?;
    let previous_row =
        roomlerd::enrollment::retire_key_of_previous_row(previous.as_ref(), &mut cfg, &outcome);
    Ok(Enrolled {
        cfg,
        outcome,
        previous_row,
    })
}

/// Finish a folded config before it is saved: a fresh install's defaults on
/// a fresh config only, the touched Advanced options always, the untouched
/// ones on a fresh config only. Returns a note for the person when the
/// options were set aside (an added org).
pub fn finish(
    cfg: &mut AgentConfig,
    outcome: &EnrollOutcome,
    service_install_defaults: bool,
    advanced: &AdvancedOptions,
    overlay_default: Option<bool>,
) -> Result<Option<String>, String> {
    match outcome {
        EnrollOutcome::FreshPrimary => {
            if service_install_defaults {
                roomlerd::enrollment::apply_service_install_defaults(cfg);
            }
            advanced
                .clone()
                .with_overlay_default(overlay_default)
                .apply_to(cfg, false)?;
            Ok(None)
        }
        EnrollOutcome::RefreshedPrimary | EnrollOutcome::ReplacedPrimary => {
            // The device's own values stay; only what the person touched
            // lands on top.
            advanced.apply_to(cfg, true)?;
            Ok(None)
        }
        EnrollOutcome::RefreshedOrg { label } | EnrollOutcome::AppendedOrg { label } => {
            Ok(Some(format!(
                "this machine is already enrolled in another organization; the new one was added as org \"{label}\" (manage with `roomlerd org ls`). The Advanced options apply to the primary enrollment and were not changed."
            )))
        }
    }
}

/// The keys the person touched, for a merge that runs elsewhere (the
/// privileged half's `--config-take`).
pub fn touched_keys(advanced: &AdvancedOptions) -> Vec<String> {
    advanced.touched_keys()
}

#[cfg(test)]
mod tests {
    use super::*;
    use roomler_ai_remote_control::models::LockOnEnd;

    fn existing() -> AgentConfig {
        let mut cfg = roomlerd::config::test_fixture();
        cfg.overlay_enabled = true;
        cfg.auto_grant_session = false;
        cfg.lock_on_end = LockOnEnd::Always;
        cfg
    }

    fn touched(overlay: Option<bool>, touched: &[&str]) -> AdvancedOptions {
        AdvancedOptions {
            overlay_enabled: overlay,
            advertise_local_subnets: Some(true),
            auto_grant_session: Some(true),
            overlay_advertised_routes: None,
            advertise_routes: None,
            touched: touched.iter().map(|k| k.to_string()).collect(),
        }
    }

    /// A fresh config takes the role's overlay default and the service
    /// install's defaults; every box lands as the screen showed it.
    #[test]
    fn a_fresh_config_takes_the_defaults_and_every_box() {
        let mut cfg = roomlerd::config::test_fixture();
        cfg.overlay_enabled = false;
        cfg.auto_grant_session = false;
        let note = finish(
            &mut cfg,
            &EnrollOutcome::FreshPrimary,
            true,
            &touched(None, &[]),
            Some(true),
        )
        .unwrap();
        assert!(note.is_none());
        assert!(cfg.overlay_enabled, "the role default");
        assert!(
            cfg.auto_grant_session,
            "an untouched box still lands on a fresh config"
        );
        assert_eq!(
            cfg.lock_on_end,
            LockOnEnd::Auto,
            "the service install's default"
        );
    }

    /// #2338 — a refreshed config keeps its own values against an untouched
    /// box and against a fresh install's defaults.
    #[test]
    fn a_refreshed_config_keeps_what_the_device_had() {
        let mut cfg = existing();
        finish(
            &mut cfg,
            &EnrollOutcome::RefreshedPrimary,
            true,
            &touched(Some(false), &[]),
            Some(false),
        )
        .unwrap();
        assert!(
            cfg.overlay_enabled,
            "overlay_enabled survived an untouched box that showed off"
        );
        assert!(!cfg.auto_grant_session, "auto_grant_session survived");
        assert_eq!(
            cfg.lock_on_end,
            LockOnEnd::Always,
            "lock_on_end was not reset to auto"
        );
    }

    /// A box the person touched lands on a refreshed config too — and only
    /// that one.
    #[test]
    fn a_touched_box_lands_on_a_refreshed_config() {
        let mut cfg = existing();
        finish(
            &mut cfg,
            &EnrollOutcome::RefreshedPrimary,
            false,
            &touched(Some(false), &["overlay_enabled"]),
            None,
        )
        .unwrap();
        assert!(!cfg.overlay_enabled, "the touched box");
        assert!(!cfg.auto_grant_session, "the untouched one did not land");
    }

    /// An added org: the primary's options are left alone, and the person is
    /// told.
    #[test]
    fn an_added_org_changes_no_primary_option() {
        let mut cfg = existing();
        let note = finish(
            &mut cfg,
            &EnrollOutcome::AppendedOrg {
                label: "other".into(),
            },
            true,
            &touched(Some(false), &["overlay_enabled", "auto_grant_session"]),
            Some(false),
        )
        .unwrap();
        assert!(cfg.overlay_enabled);
        assert!(!cfg.auto_grant_session);
        assert_eq!(cfg.lock_on_end, LockOnEnd::Always);
        assert!(note.unwrap().contains("\"other\""));
    }

    /// F4 — the wizard's fold (Windows, Linux and macOS per-user) never reads
    /// a config that is there and does not load as "absent". Before the fix it
    /// took the probe's `None` and wrote a fresh config over the device's
    /// settings, unsaid; `save` does not even keep a file that does not parse.
    #[test]
    fn a_config_that_does_not_load_is_said_recovered_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // Nothing there, and a config that loads: no word.
        let none = read_existing(&path);
        assert!(none.cfg.is_none() && none.warning.is_none());
        roomlerd::config::save(&path, &existing()).unwrap();
        let fine = read_existing(&path);
        assert!(fine.cfg.is_some() && fine.warning.is_none());

        // A second save keeps the first as `.prev`; then a hand edit breaks it.
        roomlerd::config::save(&path, &existing()).unwrap();
        std::fs::write(&path, "overlay_enabled = ture\n").unwrap();
        let broken = read_existing(&path);
        let warning = broken.warning.expect("said");
        assert!(warning.contains("could not be read"), "{warning}");
        assert!(warning.contains("config.toml.prev"), "{warning}");
        assert!(warning.contains("config.toml.unreadable"), "{warning}");
        let cfg = broken.cfg.expect("its last good copy");
        assert!(cfg.overlay_enabled, "the device's settings carry over");
        assert_eq!(cfg.lock_on_end, LockOnEnd::Always);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.toml.unreadable")).unwrap(),
            "overlay_enabled = ture\n",
            "the damaged file is kept"
        );

        // No last good copy: said, kept, and nothing to fold into.
        std::fs::remove_file(dir.path().join("config.toml.prev")).unwrap();
        let lost = read_existing(&path);
        assert!(lost.cfg.is_none());
        assert!(
            lost.warning.expect("said").contains("do not carry over"),
            "the person is told the settings are gone"
        );
    }

    /// F3 — the in-process fold retires the WireGuard key when the server
    /// answered with another device row of the same org, and keeps it on the
    /// same row (the ordinary reinstall).
    #[test]
    fn a_new_row_retires_the_key_in_the_wizards_fold() {
        let mut held = existing();
        held.overlay_wg_secret_key = Some("OLD".into());
        let mut same = roomlerd::config::test_fixture();
        same.agent_token = "NEW".into();
        let kept = fold(Some(held.clone()), same).unwrap();
        assert!(kept.previous_row.is_none());
        assert_eq!(kept.cfg.overlay_wg_secret_key.as_deref(), Some("OLD"));

        let mut other = roomlerd::config::test_fixture();
        other.agent_id = "aid-2".into();
        other.machine_id = "mid-2".into();
        let moved = fold(Some(held), other).unwrap();
        assert_eq!(moved.cfg.overlay_wg_secret_key, None);
        assert_eq!(moved.previous_row.expect("a new row").agent_id, "aid");
        assert!(moved.cfg.overlay_enabled, "everything else stays");
    }
}

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

use std::path::Path;

use roomlerd::config::AgentConfig;
use roomlerd::enrollment::{EnrollInputs, EnrollOutcome, InstallInfo};

use crate::orchestrator_agent::AdvancedOptions;

/// A folded enrollment: the config to save, and what the fold did.
pub struct Enrolled {
    pub cfg: AgentConfig,
    pub outcome: EnrollOutcome,
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
    let (cfg, outcome) = roomlerd::enrollment::apply_enrollment(existing, fresh, None, false)?;
    Ok(Enrolled { cfg, outcome })
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
}

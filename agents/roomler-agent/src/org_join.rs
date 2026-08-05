//! Multi-org — join an additional org from the admin UI
//! (`rc:agent.join_org`), without touching the machine.
//!
//! The CLI path (`roomler enroll --label …`) needs someone at the keyboard,
//! which is exactly what a corp-managed box or a headless VM doesn't have.
//! This module is the remote twin: the server pushes a single-use enrollment
//! token for the TARGET tenant down the PRIMARY org's socket, and the agent
//! does the rest itself —
//!
//! 1. exchange the token for an agent JWT in that tenant (against the server
//!    it is ALREADY talking to — the frame carries no URL, so a forged or
//!    relayed message can never repoint the device),
//! 2. fold the result into the config as a new `[[orgs]]` entry via the same
//!    [`apply_enrollment`](crate::enrollment::apply_enrollment) dispatch the
//!    CLI uses — freshly minted per-org WG key included,
//! 3. bring that org's supervised WS loop up **in-process**, so the device
//!    appears online in the new org within seconds instead of at the next
//!    daemon restart.
//!
//! Step 3 is why [`OrgSpawner`] exists: `run_cmd` builds the shared handles
//! (encoder preference, shutdown signal, consent broker) once at startup and
//! installs them here, so a join arriving hours later can spawn a supervisor
//! that is indistinguishable from one started at boot.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::config::{AgentConfig, OrgEntry, OrgOverlayMode};
use crate::enrollment::{EnrollInputs, EnrollOutcome, apply_enrollment};

/// What `run_cmd` hands over so a join arriving hours later can write the
/// config the daemon actually loaded and start a supervisor for the result.
///
/// A static rather than nine more parameters on `signaling::run`: the
/// handler is three call-frames deep in the message loop, and every field
/// here is a process-wide singleton anyway (one config path, one write
/// lock, one supervisor factory).
pub struct JoinRuntime {
    /// The config path THIS daemon resolved at startup (perUser vs
    /// machine-global vs `--config`) — never re-resolved, or a
    /// SystemContext daemon would write the wrong profile's file.
    pub config_path: std::path::PathBuf,
    /// The daemon-wide config write lock (P6), shared with the CLI
    /// re-enroll / updater / route-reconciler writers.
    pub write_lock: Arc<tokio::sync::Mutex<()>>,
    /// Spawns a supervised signaling loop for a freshly appended org.
    pub spawn_org: Box<dyn Fn(OrgEntry) + Send + Sync>,
}

static JOIN_RUNTIME: std::sync::OnceLock<JoinRuntime> = std::sync::OnceLock::new();

/// Install the join runtime (once, from `run_cmd`). Without it — CLI verbs,
/// tests — a pushed join is refused loudly instead of half-applied.
pub fn install(runtime: JoinRuntime) {
    let _ = JOIN_RUNTIME.set(runtime);
}

/// Handle a pushed `rc:agent.join_org` using the installed runtime.
/// `cfg` is the calling loop's config (its `server_url` / `machine_id` are
/// what the enrollment is performed with).
pub async fn join_from_push(
    cfg: &AgentConfig,
    enrollment_token: &str,
    label: Option<&str>,
    overlay_mode: Option<&str>,
) -> Result<JoinOutcome> {
    let rt = JOIN_RUNTIME
        .get()
        .context("rc:agent.join_org received but no join runtime is installed")?;
    join_org(
        cfg,
        &rt.config_path,
        rt.write_lock.clone(),
        enrollment_token,
        label,
        overlay_mode,
    )
    .await
}

/// Outcome of a remote join, for the log line and the tests.
#[derive(Debug, PartialEq, Eq)]
pub enum JoinOutcome {
    /// A new `[[orgs]]` entry was appended (and supervised if possible).
    Joined { label: String, supervised: bool },
    /// The device was already enrolled in that org; its token/agent-id were
    /// refreshed in place. Not an error — the admin clicked twice, or a
    /// stale row was re-pushed.
    Refreshed { label: String },
    /// The enrollment resolved to the PRIMARY identity. Refused: a remote
    /// push must never be able to rewrite the machine's primary enrollment
    /// (that is what `enroll --replace` at the keyboard is for).
    RefusedPrimaryRebind,
}

/// Perform a pushed join end to end. `server_url` is the agent's OWN
/// (`cfg.server_url`), never a value off the wire.
pub async fn join_org(
    cfg: &AgentConfig,
    config_path: &std::path::Path,
    write_lock: Arc<tokio::sync::Mutex<()>>,
    enrollment_token: &str,
    label: Option<&str>,
    overlay_mode: Option<&str>,
) -> Result<JoinOutcome> {
    let fresh = crate::enrollment::enroll(EnrollInputs {
        server_url: &cfg.server_url,
        enrollment_token,
        machine_id: &cfg.machine_id,
        machine_name: &cfg.machine_name,
    })
    .await
    .context("exchanging the pushed enrollment token")?;

    // Serialize with every other config writer (CLI re-enroll, updater,
    // route reconciler) — the P6 daemon-wide write lock.
    let _guard = write_lock.lock().await;

    let existing = crate::config::load(&config_path.to_path_buf()).ok();
    let had_config = existing.is_some();
    let (mut merged, outcome) = apply_enrollment(existing, fresh, label, false)?;

    let joined_label = match &outcome {
        EnrollOutcome::AppendedOrg { label } => label.clone(),
        EnrollOutcome::RefreshedOrg { label } => {
            info!(org = %label, "join_org: already enrolled; refreshed in place");
            crate::config::save(&config_path.to_path_buf(), &merged)?;
            return Ok(JoinOutcome::Refreshed {
                label: label.clone(),
            });
        }
        // A pushed token that resolves to this machine's PRIMARY identity
        // would rewrite the primary enrollment wholesale. Refuse: nothing
        // is written, and the operator gets a truthful log.
        EnrollOutcome::RefreshedPrimary | EnrollOutcome::ReplacedPrimary => {
            warn!(
                "rc:agent.join_org refused — the token resolves to this machine's \
                 PRIMARY enrollment; a remote push may not rebind it"
            );
            return Ok(JoinOutcome::RefusedPrimaryRebind);
        }
        EnrollOutcome::FreshPrimary => {
            // No config on disk at all. A running daemon always has one, so
            // this means the path moved under us — refuse rather than write
            // a brand-new primary from a pushed token.
            if !had_config {
                bail!("join_org: no existing config at {}", config_path.display());
            }
            unreachable!("apply_enrollment only returns FreshPrimary without an existing config")
        }
    };

    // Overlay participation for the new org, when the admin asked for it.
    // `tun` additionally needs the daemon's own `overlay_multi_org` opt-in
    // to take effect (`AgentConfig::for_org`), so setting it here is
    // declarative, never a bypass.
    if let Some(mode) = overlay_mode
        && let Some(entry) = merged.orgs.iter_mut().find(|o| o.label == joined_label)
    {
        entry.overlay_mode = match mode {
            "tun" => OrgOverlayMode::Tun,
            "netstack" => OrgOverlayMode::Netstack,
            _ => OrgOverlayMode::Off,
        };
    }

    crate::config::save(&config_path.to_path_buf(), &merged)?;

    let entry = merged
        .orgs
        .iter()
        .find(|o| o.label == joined_label)
        .cloned()
        .context("appended org vanished from the merged config")?;
    drop(_guard);

    let supervised = match JOIN_RUNTIME.get() {
        Some(rt) => {
            (rt.spawn_org)(entry);
            true
        }
        None => {
            warn!(
                org = %joined_label,
                "join_org: no supervisor factory installed; the new org connects \
                 at the next daemon start"
            );
            false
        }
    };
    info!(org = %joined_label, supervised, "rc:agent.join_org — joined a new org");
    Ok(JoinOutcome::Joined {
        label: joined_label,
        supervised,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PRIMARY_ORG_LABEL;

    fn base() -> AgentConfig {
        let mut c = crate::config::test_fixture();
        c.server_url = "https://roomler.ai".into();
        c.tenant_id = "tid-primary".into();
        c.machine_id = "machine-1".into();
        c
    }

    fn fresh_for(tenant: &str) -> AgentConfig {
        let mut c = base();
        c.tenant_id = tenant.to_string();
        c.agent_token = format!("tok-{tenant}");
        c.agent_id = format!("aid-{tenant}");
        c
    }

    /// The append path: a new tenant becomes a labelled secondary with its
    /// OWN WG key — never the primary's (cross-org pubkey correlation).
    #[test]
    fn a_new_tenant_appends_a_secondary_with_its_own_key() {
        let mut existing = base();
        existing.overlay_wg_secret_key = Some("PRIMARY-KEY".into());
        let (merged, outcome) =
            apply_enrollment(Some(existing), fresh_for("tid-acme"), Some("acme"), false).unwrap();
        assert_eq!(
            outcome,
            EnrollOutcome::AppendedOrg {
                label: "acme".into()
            }
        );
        let entry = merged.orgs.iter().find(|o| o.label == "acme").unwrap();
        assert_eq!(entry.tenant_id, "tid-acme");
        assert!(entry.overlay_wg_secret_key.is_some());
        assert_ne!(
            entry.overlay_wg_secret_key.as_deref(),
            Some("PRIMARY-KEY"),
            "a secondary must never borrow the primary's WG key"
        );
        // The primary identity is untouched.
        assert_eq!(merged.tenant_id, "tid-primary");
    }

    /// A token that resolves to the machine's PRIMARY identity must not
    /// rewrite it — the outcome the handler refuses on.
    #[test]
    fn a_primary_identity_token_is_recognised_as_a_rebind() {
        let existing = base();
        let (_merged, outcome) =
            apply_enrollment(Some(existing), fresh_for("tid-primary"), None, false).unwrap();
        assert_eq!(outcome, EnrollOutcome::RefreshedPrimary);
        assert_ne!(PRIMARY_ORG_LABEL, "acme");
    }

    /// Re-pushing the same org refreshes in place instead of appending a
    /// duplicate (the admin clicked twice).
    #[test]
    fn re_joining_the_same_org_refreshes_in_place() {
        let mut existing = base();
        let (merged, _) = apply_enrollment(
            Some(existing.clone()),
            fresh_for("tid-acme"),
            Some("acme"),
            false,
        )
        .unwrap();
        existing = merged;
        assert_eq!(existing.orgs.len(), 1);
        let (again, outcome) =
            apply_enrollment(Some(existing), fresh_for("tid-acme"), Some("acme"), false).unwrap();
        assert_eq!(
            outcome,
            EnrollOutcome::RefreshedOrg {
                label: "acme".into()
            }
        );
        assert_eq!(again.orgs.len(), 1, "no duplicate entry");
    }
}

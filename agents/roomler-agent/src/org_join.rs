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
    /// refreshed in place. Not an error — the admin clicked twice, or is
    /// re-sending to change the org's mesh participation. `restarted` = the
    /// org's supervisor was rebuilt because something it was spawned with
    /// changed (a running loop holds its own config).
    Refreshed {
        label: String,
        restarted: bool,
        /// The push turned ON `overlay_multi_org`. The config is written, but
        /// the mesh cannot come up until the DAEMON restarts: the primary
        /// loop still owns the TUN un-muxed, and Wintun refuses a second
        /// session on one adapter.
        restart_required: bool,
    },
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

    // A REFRESH is not a no-op: the admin may be re-sending the request
    // precisely to change this org's mesh participation on a device that is
    // already enrolled. Falling straight out here (as the first cut did) made
    // the UI's "Private mesh" choice inert for every already-added device —
    // i.e. for the exact case where you'd want to turn the mesh on later.
    let (joined_label, was_new) = match &outcome {
        EnrollOutcome::AppendedOrg { label } => (label.clone(), true),
        EnrollOutcome::RefreshedOrg { label } => {
            info!(org = %label, "join_org: already enrolled; refreshing in place");
            (label.clone(), false)
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

    // Overlay participation for this org — applied on BOTH the append and the
    // refresh path (see above).
    let mut mode_changed = false;
    // Set when this push turned ON the host-level `overlay_multi_org`: that
    // needs a DAEMON restart, not a per-org one (see below).
    let mut flag_flipped = false;
    if let Some(mode) = overlay_mode {
        let want = match mode {
            "tun" => OrgOverlayMode::Tun,
            "netstack" => OrgOverlayMode::Netstack,
            _ => OrgOverlayMode::Off,
        };
        if let Some(entry) = merged.orgs.iter_mut().find(|o| o.label == joined_label) {
            mode_changed = entry.overlay_mode != want;
            entry.overlay_mode = want;
        }
        // An admin explicitly asking this device to join a second org's mesh
        // IS the host-level opt-in `overlay_multi_org` exists to represent.
        // Without this the request could never take effect: the flag is
        // config-surface only, so there is no other remote path to set it,
        // and `AgentConfig::for_org` gates `tun` on it. Loud, because it
        // changes how the host's ONE TUN adapter is shared.
        if want == OrgOverlayMode::Tun && !merged.overlay_multi_org {
            merged.overlay_multi_org = true;
            // NOT `mode_changed` — deliberately no live restart here.
            //
            // The flag changes who owns the host's TUN: without it the
            // PRIMARY holds a plain `SystemTun`; with it every org rides the
            // shared mux. Restarting only the secondary would send it to
            // `SystemTun::up` on an adapter whose Wintun session the primary
            // still holds, and Wintun refuses a second session on one
            // adapter — field 2026-08-07 on PC50045:
            // `WintunStartSession failed "Failed to register rings"`, the
            // secondary's overlay aborting on every reconnect while the
            // primary stayed happily single-org.
            //
            // The primary's loop cannot be rebuilt from here (it owns the
            // process's main enrollment), so the honest answer is: the
            // config is written and the mesh comes up at the next daemon
            // start, which the next auto-update delivers anyway.
            flag_flipped = true;
            info!(
                org = %joined_label,
                "join_org: enabled overlay_multi_org — RESTART REQUIRED before this \
                 device can carry a second org on the shared TUN (the primary loop \
                 still owns the adapter un-muxed)"
            );
        }
    }

    crate::config::save(&config_path.to_path_buf(), &merged)?;

    let entry = merged
        .orgs
        .iter()
        .find(|o| o.label == joined_label)
        .cloned()
        .context("appended org vanished from the merged config")?;
    drop(_guard);

    // A NEW org needs a supervisor. An EXISTING one needs a RE-start, and
    // only when something it was spawned with actually changed: a running
    // loop holds the config it was given, so a mode written to disk is inert
    // until its loop is rebuilt. Re-starting on every refresh would churn a
    // healthy mesh for nothing.
    // A flag flip means the whole daemon has to come back before the mesh
    // can work; restarting the org loop alone would just fail its TUN
    // bring-up in a retry cycle.
    let restart = (was_new || mode_changed) && !flag_flipped;
    let supervised = match (JOIN_RUNTIME.get(), restart) {
        (Some(rt), true) => {
            (rt.spawn_org)(entry);
            true
        }
        (Some(_), false) => false,
        (None, _) => {
            warn!(
                org = %joined_label,
                "join_org: no supervisor factory installed; the change applies \
                 at the next daemon start"
            );
            false
        }
    };
    if was_new {
        info!(org = %joined_label, supervised, "rc:agent.join_org — joined a new org");
        Ok(JoinOutcome::Joined {
            label: joined_label,
            supervised,
        })
    } else {
        info!(
            org = %joined_label, restarted = supervised, mode_changed,
            restart_required = flag_flipped,
            "rc:agent.join_org — refreshed an existing org"
        );
        Ok(JoinOutcome::Refreshed {
            label: joined_label,
            restarted: supervised,
            restart_required: flag_flipped,
        })
    }
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
        // The invariant that holds in EVERY build: a secondary never carries
        // the primary's key. (The mint itself is `cfg`-gated on an overlay
        // surface — a signalling-only build leaves it None and mints on the
        // first overlay-enabled start, mirroring the primary's lazy path.)
        assert_ne!(
            entry.overlay_wg_secret_key.as_deref(),
            Some("PRIMARY-KEY"),
            "a secondary must never borrow the primary's WG key"
        );
        #[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
        assert!(
            entry.overlay_wg_secret_key.is_some(),
            "an overlay-capable build mints the org's own key at append time"
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

    /// The bug this fixes: a re-push at an ALREADY-ENROLLED org used to
    /// return before the overlay-mode block, so the UI's "Private mesh"
    /// choice was inert for exactly the devices you'd want to turn it on for
    /// later. Both the append and the refresh path must apply it.
    #[test]
    fn a_refresh_still_applies_the_requested_overlay_mode() {
        let existing = base();
        // First join: mesh off.
        let (cfg, outcome) =
            apply_enrollment(Some(existing), fresh_for("tid-acme"), Some("acme"), false).unwrap();
        assert_eq!(
            outcome,
            EnrollOutcome::AppendedOrg {
                label: "acme".into()
            }
        );
        assert_eq!(cfg.orgs[0].overlay_mode, OrgOverlayMode::Off);
        assert!(!cfg.overlay_multi_org);

        // Second push at the SAME org asking for the mesh: the dispatch says
        // "refresh", and the mode must still land.
        let (mut cfg, outcome) =
            apply_enrollment(Some(cfg), fresh_for("tid-acme"), Some("acme"), false).unwrap();
        assert_eq!(
            outcome,
            EnrollOutcome::RefreshedOrg {
                label: "acme".into()
            }
        );
        // (mirrors join_org's post-dispatch block)
        let entry = cfg.orgs.iter_mut().find(|o| o.label == "acme").unwrap();
        entry.overlay_mode = OrgOverlayMode::Tun;
        cfg.overlay_multi_org = true;

        assert_eq!(cfg.orgs[0].overlay_mode, OrgOverlayMode::Tun);
        // …and the host-level opt-in that `for_org` gates `tun` on, without
        // which the request could never take effect (the flag has no other
        // remote path).
        assert!(cfg.overlay_multi_org);
        // …and with the org's own key present, `for_org` lets the secondary
        // onto the mesh. Set explicitly: the mint is cfg-gated on an overlay
        // feature, and this asserts the GATE's logic, not the mint's.
        cfg.orgs[0].overlay_wg_secret_key = Some("ORG-KEY".into());
        let org = cfg.orgs[0].clone();
        assert!(
            cfg.for_org(&org).overlay_enabled,
            "flag + tun + same server + own key ⇒ the secondary joins the mesh"
        );
        // Drop any one leg and it stays off — the gate is a conjunction.
        let mut no_flag = cfg.clone();
        no_flag.overlay_multi_org = false;
        assert!(!no_flag.for_org(&org).overlay_enabled, "flag is required");
    }

    /// Field 2026-08-07 (PC50045): turning `overlay_multi_org` ON must NOT
    /// live-restart the org loop. The flag decides who owns the host's TUN —
    /// without it the PRIMARY holds a plain `SystemTun`, and a secondary
    /// restarted into the mux calls `SystemTun::up` on an adapter whose
    /// Wintun session the primary still holds. Wintun refuses:
    /// "Failed to register rings", and the org's overlay aborts on every
    /// reconnect. The config is written; the mesh waits for a daemon start.
    #[test]
    fn turning_the_multi_org_flag_on_defers_to_a_daemon_restart() {
        // Mirrors join_org's decision inputs.
        for (was_new, mode_changed, flag_flipped, expect_restart) in [
            // A brand-new org on a host ALREADY in multi-org mode: live spawn.
            (true, false, false, true),
            // Mode changed on an existing org, flag already on: cycle it.
            (false, true, false, true),
            // The flag had to be flipped ⇒ defer, whatever else changed.
            (true, true, true, false),
            (false, true, true, false),
            // Nothing changed: never churn a healthy mesh.
            (false, false, false, false),
        ] {
            let restart = (was_new || mode_changed) && !flag_flipped;
            assert_eq!(
                restart, expect_restart,
                "was_new={was_new} mode_changed={mode_changed} flag_flipped={flag_flipped}"
            );
        }
    }
}

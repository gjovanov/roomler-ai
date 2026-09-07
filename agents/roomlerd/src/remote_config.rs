// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Applying a pushed desired-config (`docs/remote-config.md` step 4).
//!
//! Rides into `signaling::run` as one value rather than three loose
//! parameters, the same way `ssh::SessionServices` rides through
//! `overlay::maybe_start` — and for the same reason: "no services ⇒ cannot
//! apply" should be unrepresentable rather than a `None` check somebody
//! forgets.
//!
//! # Why `exec_enabled` is live and `ssh_enabled` is not
//!
//! `signaling::run` takes an OWNED `AgentConfig` snapshot at startup, so a key
//! read from it can never change in a running daemon. Persisting to disk alone
//! would leave the dashboard reporting a change that does nothing until
//! somebody restarts the box — worse than not shipping, because it reads as
//! applied.
//!
//! `exec_enabled` escapes that: it has exactly ONE read site (the `rc:rpc.exec`
//! gate, checked per request), so routing it through an `AtomicBool` makes it
//! genuinely live. Flipping it takes effect on the next command, with no
//! restart and therefore no risk of taking a host offline.
//!
//! `ssh_enabled` does NOT escape it. The SSH server is spliced into the packet
//! path at overlay-runtime build time (`ssh::maybe_intercept` → `SplitTun`),
//! and `overlay::RuntimeFingerprint` — which decides whether a respawned
//! runtime re-attaches or rebuilds — has no SSH field, so a flipped value does
//! not even trigger a rebuild. It is persisted here and honestly reported as
//! needing a restart, rather than pretended into effect.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use roomler_ai_remote_control::models::DesiredConfig;

/// What applying a desired-config actually achieved. Split because the two
/// halves are genuinely different and an operator needs to know which they
/// got: one is in force now, the other is written down and waiting.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Keys that changed and are ALREADY in force.
    pub live: Vec<&'static str>,
    /// Keys that changed on disk but need a daemon restart to take effect.
    pub needs_restart: Vec<&'static str>,
}

impl Applied {
    pub fn is_noop(&self) -> bool {
        self.live.is_empty() && self.needs_restart.is_empty()
    }
}

#[derive(Clone)]
pub struct RemoteConfigServices {
    path: PathBuf,
    lock: crate::config::WriteLock,
    /// The LIVE `exec_enabled`, read by the `rc:rpc.exec` gate. Seeded from
    /// the startup config and the only copy that decides anything.
    exec_enabled: Arc<AtomicBool>,
    /// The LIVE opt-in, read by the `rc:agent.config` handler.
    ///
    /// Live for a reason that is the whole point of the key: it is how the
    /// person holding the machine REVOKES the delegation. A revocation that
    /// only took effect after a service restart — while the control plane's
    /// assertions took effect immediately — would be a strange kind of last
    /// word. The server can never write this; only [`Self::adopt_local`] can.
    remote_config_enabled: Arc<AtomicBool>,
}

impl RemoteConfigServices {
    pub fn new(
        path: PathBuf,
        lock: crate::config::WriteLock,
        exec_enabled: bool,
        remote_config_enabled: bool,
    ) -> Self {
        Self {
            path,
            lock,
            exec_enabled: Arc::new(AtomicBool::new(exec_enabled)),
            remote_config_enabled: Arc::new(AtomicBool::new(remote_config_enabled)),
        }
    }

    /// Gate 4 for Fleet RPC, read per request.
    pub fn exec_enabled(&self) -> bool {
        self.exec_enabled.load(Ordering::Relaxed)
    }

    /// Does this device accept pushed config at all? Read per push.
    pub fn remote_config_enabled(&self) -> bool {
        self.remote_config_enabled.load(Ordering::Relaxed)
    }

    /// Re-seed the live flags from a config the LOCAL owner just wrote.
    ///
    /// Called by the LocalAPI's `ConfigSet` (the desktop companion, `roomler
    /// config set`) after a successful save, and it closes a genuine inversion:
    /// [`Self::apply`] made a SERVER push to `exec_enabled` take effect
    /// immediately, while an owner's own edit sat in the file until the daemon
    /// restarted. Gate 4 exists so the person holding the machine has the last
    /// word — it cannot be the slower of the two.
    ///
    /// ⚠️ This does NOT stop the next reconnect from re-applying a standing
    /// `desired_config`, and it should not: a device with
    /// `remote_config_enabled = true` has delegated the key, which is the
    /// documented bargain (`docs/remote-config.md` §2). The owner's real
    /// remedy is to turn the OPT-IN off — which is exactly why that one is
    /// live here too, and why turning it off takes effect before the next push
    /// rather than after a restart.
    ///
    /// The caller already holds the write lock, so this reads a config that
    /// cannot be mid-write.
    pub fn adopt_local(&self, cfg: &crate::config::AgentConfig) {
        self.exec_enabled.store(cfg.exec_enabled, Ordering::Relaxed);
        self.remote_config_enabled
            .store(cfg.remote_config_enabled, Ordering::Relaxed);
    }

    /// Reconcile the on-disk config against `desired`, persist, and put the
    /// live keys into force.
    ///
    /// Idempotent by construction: a desired state that already matches writes
    /// NOTHING and returns an empty [`Applied`]. That matters more than it
    /// looks — delivery is reconcile-on-connect, so this runs on every single
    /// reconnect, and a version that wrote unconditionally would rewrite
    /// config.toml (and later, restart the daemon) every time a flaky link
    /// bounced.
    ///
    /// Reloads from DISK rather than mutating the startup snapshot: another
    /// writer (the CLI, the desktop companion) may have changed the file since
    /// this process started, and clobbering their edit is not ours to do.
    pub async fn apply(&self, desired: &DesiredConfig) -> Result<Applied, String> {
        let _guard = self.lock.lock().await;
        let path = self.path.clone();
        let desired = desired.clone();
        let exec_flag = self.exec_enabled.clone();

        tokio::task::spawn_blocking(move || {
            let mut cfg =
                crate::config::load(&path).map_err(|e| format!("loading config: {e:#}"))?;
            let mut applied = Applied::default();

            if let Some(want) = desired.exec_enabled
                && cfg.exec_enabled != want
            {
                cfg.exec_enabled = want;
                applied.live.push("exec_enabled");
            }
            if let Some(want) = desired.ssh_enabled
                && cfg.ssh_enabled != want
            {
                cfg.ssh_enabled = want;
                applied.needs_restart.push("ssh_enabled");
            }
            if let Some(want) = desired.ssh_authorized_keys.as_ref()
                && &cfg.ssh_authorized_keys != want
            {
                cfg.ssh_authorized_keys = want.clone();
                applied.needs_restart.push("ssh_authorized_keys");
            }
            if let Some(want) = desired.ssh_account_mode.as_ref()
                && cfg.ssh_account_mode.as_ref() != Some(want)
            {
                cfg.ssh_account_mode = Some(want.clone());
                applied.needs_restart.push("ssh_account_mode");
            }
            if let Some(want) = desired.ssh_port
                && cfg.ssh_port != Some(want)
            {
                cfg.ssh_port = Some(want);
                applied.needs_restart.push("ssh_port");
            }
            // FR-77 P3 — the cell denylist. A blank push clears the key back
            // to the built-in list; `none` denies nothing. Read once per
            // process by the probe child, hence `needs_restart`.
            if let Some(want) = desired.encoder_cells_deny.as_deref() {
                let next = Some(want.trim())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                if cfg.encoder_cells_deny != next {
                    cfg.encoder_cells_deny = next;
                    applied.needs_restart.push("encoder_cells_deny");
                }
            }

            if applied.is_noop() {
                return Ok(applied);
            }
            crate::config::save(&path, &cfg).map_err(|e| format!("saving config: {e:#}"))?;
            // AFTER the durable write, never before: a live flag that is on
            // while the file says off would survive a crash as a device
            // running exec that its own config denies.
            if applied.live.contains(&"exec_enabled") {
                exec_flag.store(cfg.exec_enabled, Ordering::Relaxed);
            }
            Ok(applied)
        })
        .await
        .unwrap_or_else(|e| Err(format!("config apply task join: {e}")))
    }

    /// FR-40 — persist a freshly minted overlay key for ONE enrollment and
    /// bump its epoch, under the daemon-wide write lock, through the same
    /// atomic `config::save` as everything else (fsync + `.prev` + 0600/ACL).
    ///
    /// `is_primary` selects the config's scalar identity; otherwise the
    /// `[[orgs]]` entry whose `tenant_id` matches. Returns the new epoch.
    /// The caller adopts the key in memory ONLY after this succeeds — a key
    /// that is not written down would be lost at the next restart, and the
    /// device would come back as the identity it just retired.
    pub async fn rotate_overlay_key(
        &self,
        is_primary: bool,
        tenant_id: &str,
        new_secret: String,
    ) -> Result<u32, String> {
        let _guard = self.lock.lock().await;
        let path = self.path.clone();
        let tenant_id = tenant_id.to_string();

        tokio::task::spawn_blocking(move || {
            let mut cfg =
                crate::config::load(&path).map_err(|e| format!("loading config: {e:#}"))?;
            let epoch = if is_primary {
                cfg.overlay_wg_secret_key = Some(new_secret);
                cfg.overlay_wg_key_epoch = cfg.overlay_wg_key_epoch.wrapping_add(1);
                cfg.overlay_wg_key_epoch
            } else {
                let org = cfg
                    .orgs
                    .iter_mut()
                    .find(|o| o.tenant_id == tenant_id)
                    .ok_or_else(|| format!("no [[orgs]] entry for tenant {tenant_id}"))?;
                org.overlay_wg_secret_key = Some(new_secret);
                org.overlay_wg_key_epoch = org.overlay_wg_key_epoch.wrapping_add(1);
                org.overlay_wg_key_epoch
            };
            crate::config::save(&path, &cfg).map_err(|e| format!("saving config: {e:#}"))?;
            Ok(epoch)
        })
        .await
        .unwrap_or_else(|e| Err(format!("key rotation task join: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(exec: bool) -> RemoteConfigServices {
        svc_with(exec, true)
    }

    fn svc_with(exec: bool, opted_in: bool) -> RemoteConfigServices {
        RemoteConfigServices::new(
            PathBuf::from("unused-in-this-test.toml"),
            Arc::new(tokio::sync::Mutex::new(())),
            exec,
            opted_in,
        )
    }

    /// FR-40 — a rotation writes ONE enrollment's key + epoch through the
    /// atomic save and leaves the other identity on disk untouched; an
    /// unknown org is refused with nothing written.
    #[tokio::test]
    async fn rotate_overlay_key_moves_one_identity_and_bumps_its_epoch() {
        let dir = std::env::temp_dir().join(format!("fr40-rotate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut cfg = crate::config::test_fixture();
        cfg.overlay_wg_secret_key = Some("PRIMARY-OLD".into());
        cfg.overlay_wg_key_epoch = 4;
        cfg.orgs.push(crate::config::OrgEntry {
            label: "beta".into(),
            server_url: cfg.server_url.clone(),
            ws_url: None,
            agent_token: "tok-beta".into(),
            agent_id: "aid-beta".into(),
            tenant_id: "tid-beta".into(),
            enabled: true,
            overlay_mode: crate::config::OrgOverlayMode::Off,
            overlay_wg_secret_key: Some("BETA-OLD".into()),
            overlay_wg_key_epoch: 0,
            overlay_advertised_routes: Vec::new(),
            overlay_exit_node_enabled: false,
            advertise_routes: Vec::new(),
            netstack_socks_port: None,
        });
        crate::config::save(&path, &cfg).unwrap();
        let svc = RemoteConfigServices::new(
            path.clone(),
            Arc::new(tokio::sync::Mutex::new(())),
            false,
            true,
        );

        // A secondary org: only its entry moves.
        let epoch = svc
            .rotate_overlay_key(false, "tid-beta", "BETA-NEW".into())
            .await
            .unwrap();
        assert_eq!(epoch, 1);
        let back = crate::config::load(&path).unwrap();
        assert_eq!(
            back.orgs[0].overlay_wg_secret_key.as_deref(),
            Some("BETA-NEW")
        );
        assert_eq!(back.orgs[0].overlay_wg_key_epoch, 1);
        assert_eq!(back.overlay_wg_secret_key.as_deref(), Some("PRIMARY-OLD"));
        assert_eq!(back.overlay_wg_key_epoch, 4);

        // The primary: the scalar moves, the org stays.
        let epoch = svc
            .rotate_overlay_key(true, "tid-primary", "PRIMARY-NEW".into())
            .await
            .unwrap();
        assert_eq!(epoch, 5);
        let back = crate::config::load(&path).unwrap();
        assert_eq!(back.overlay_wg_secret_key.as_deref(), Some("PRIMARY-NEW"));
        assert_eq!(back.overlay_wg_key_epoch, 5);
        assert_eq!(
            back.orgs[0].overlay_wg_secret_key.as_deref(),
            Some("BETA-NEW")
        );

        // An org this device is not enrolled in: refused, nothing written.
        assert!(
            svc.rotate_overlay_key(false, "tid-nope", "X".into())
                .await
                .is_err()
        );
        let back = crate::config::load(&path).unwrap();
        assert_eq!(back.overlay_wg_key_epoch, 5);
        assert_eq!(back.orgs[0].overlay_wg_key_epoch, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_live_flag_is_what_the_gate_reads() {
        assert!(!svc(false).exec_enabled());
        assert!(svc(true).exec_enabled());
    }

    #[test]
    fn the_opt_in_is_seeded_from_the_config_this_daemon_loaded() {
        assert!(!svc_with(false, false).remote_config_enabled());
        assert!(svc_with(false, true).remote_config_enabled());
    }

    /// The inversion this closes.
    ///
    /// `apply` (a SERVER push) puts `exec_enabled` into force immediately. If
    /// the owner's own edit only reached the file, their "off" would wait for a
    /// service restart while the control plane's "on" did not — and gate 4
    /// exists precisely so the person holding the machine has the last word.
    /// It cannot be the slower of the two.
    #[test]
    fn an_owners_local_edit_is_at_least_as_live_as_a_pushed_one() {
        let svc = svc_with(true, true);
        assert!(svc.exec_enabled());

        let mut cfg = crate::config::test_fixture();
        cfg.exec_enabled = false;
        cfg.remote_config_enabled = false;
        svc.adopt_local(&cfg);

        assert!(!svc.exec_enabled(), "the owner turned exec off; it is off");
        assert!(
            !svc.remote_config_enabled(),
            "revoking the delegation must not wait for a restart either — \
             otherwise the server keeps pushing over a decision already made"
        );
    }

    #[test]
    fn an_empty_applied_is_a_noop() {
        assert!(Applied::default().is_noop());
        assert!(
            !Applied {
                live: vec!["exec_enabled"],
                ..Default::default()
            }
            .is_noop()
        );
        assert!(
            !Applied {
                needs_restart: vec!["ssh_enabled"],
                ..Default::default()
            }
            .is_noop()
        );
    }

    /// The split is the honest part of this module: an operator has to be able
    /// to tell "in force now" from "written down, waiting for a restart".
    /// Collapsing them would let the dashboard claim SSH is on when the daemon
    /// has not re-spliced the packet path.
    #[test]
    fn live_and_restart_required_keys_stay_separate() {
        let applied = Applied {
            live: vec!["exec_enabled"],
            needs_restart: vec!["ssh_enabled"],
        };
        assert!(!applied.is_noop());
        assert!(!applied.live.contains(&"ssh_enabled"));
        assert!(!applied.needs_restart.contains(&"exec_enabled"));
    }
}

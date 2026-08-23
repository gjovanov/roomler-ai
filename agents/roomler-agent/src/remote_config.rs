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
}

impl RemoteConfigServices {
    pub fn new(path: PathBuf, lock: crate::config::WriteLock, exec_enabled: bool) -> Self {
        Self {
            path,
            lock,
            exec_enabled: Arc::new(AtomicBool::new(exec_enabled)),
        }
    }

    /// Gate 4 for Fleet RPC, read per request.
    pub fn exec_enabled(&self) -> bool {
        self.exec_enabled.load(Ordering::Relaxed)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(exec: bool) -> RemoteConfigServices {
        RemoteConfigServices::new(
            PathBuf::from("unused-in-this-test.toml"),
            Arc::new(tokio::sync::Mutex::new(())),
            exec,
        )
    }

    #[test]
    fn the_live_flag_is_what_the_gate_reads() {
        assert!(!svc(false).exec_enabled());
        assert!(svc(true).exec_enabled());
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

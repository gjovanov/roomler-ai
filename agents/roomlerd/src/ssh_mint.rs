// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Roomler SSH (P2) — the host key a start mints for this node, and what the
//! start does when it cannot make one or cannot keep it.
//!
//! Out of `run_cmd` so a test can hand the mint a generator or a save that
//! fails. Neither fails on demand on a real host, and what a failure leaves
//! behind, in this run and in the file, is the part that has to hold (#3353).

use crate::config::AgentConfig;

/// What the start's mint did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mint {
    /// Nothing to make: SSH is off, or the stored key is usable.
    NotNeeded,
    /// A key was made and saved, and this run serves it.
    Minted,
    /// The key could not be made, or could not be saved: this run serves no
    /// SSH.
    OffThisRun,
}

/// Mint this node's SSH host identity on an SSH-enabled start whose stored key
/// is missing or unusable, the same shape as the WG key: generated locally,
/// persisted through `config::save` (atomic + fsync + `.prev` + 0600 / ACL),
/// never transmitted. A device that never enables SSH never generates one.
/// `generate` makes the key, and `save` writes the config that holds it: the
/// start's own [`StartupWrites`](crate::config::StartupWrites).
///
/// Failing to persist is FATAL to the feature rather than merely logged: a
/// host key that changes on every boot trains operators to accept unknown
/// fingerprints, which is precisely the habit that makes SSH host
/// verification worthless. Better to serve nothing than to serve a new
/// identity every restart.
///
/// ⚠️ The condition is "not USABLE", not "absent". A key that is present but
/// unparseable is the state this guard used to walk straight past, and it is
/// strictly worse than having none: the daemon refuses to serve SSH (right)
/// and publishes no host pubkey (right), and then never mints a replacement —
/// so the device is wedged permanently and every restart repeats the same
/// warning into a log nobody reads. Found in the field on a 0.4.96 corp
/// laptop whose stored PEM had picked up literal `\r` escape text; see
/// [`crate::ssh::stored_host_key_is_usable`].
///
/// Replacing it is safe precisely BECAUSE it was unreadable: the daemon never
/// served with it and never published it, so no client ever verified against
/// it. There is no identity to preserve — which is what separates this from
/// the "never serve a key that changes every restart" rule above.
pub fn at_start(
    cfg: &mut AgentConfig,
    generate: impl FnOnce() -> anyhow::Result<String>,
    save: impl FnOnce(&AgentConfig) -> anyhow::Result<()>,
) -> Mint {
    if !cfg.ssh_enabled || crate::ssh::stored_host_key_is_usable(cfg) {
        return Mint::NotNeeded;
    }
    if cfg.ssh_host_key.is_some() {
        tracing::warn!(
            "ssh: the stored host key is unreadable — replacing it. Nothing ever \
             verified against it (the daemon refused to serve SSH with it), so there \
             is no identity to preserve"
        );
        cfg.ssh_host_key = None;
    }
    match generate() {
        Ok(pem) => {
            cfg.ssh_host_key = Some(pem);
            match save(cfg) {
                Ok(()) => {
                    tracing::info!("ssh: generated + persisted node SSH host key");
                    Mint::Minted
                }
                Err(e) => {
                    cfg.ssh_host_key = None;
                    cfg.ssh_enabled = false;
                    tracing::error!(
                        error = %e,
                        "ssh: could not persist the host key — SSH stays OFF this run \
                         rather than serving an identity that changes on every restart"
                    );
                    Mint::OffThisRun
                }
            }
        }
        Err(e) => {
            cfg.ssh_enabled = false;
            tracing::error!(error = %e, "ssh: host key generation failed — SSH stays OFF");
            Mint::OffThisRun
        }
    }
}

/// #3353 — a failed mint keeps SSH off for its own run and leaves the owner's
/// `ssh_enabled` in the file, so the next start tries again. Each case runs a
/// start the way `run_cmd` does: load, mint, then the crash bookkeeping's
/// startup save of the whole in-memory copy.
#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use tunnel_core::overlay::runtime::TunFactory;
    use tunnel_core::overlay::tun::TunIo;

    use super::{Mint, at_start};
    use crate::config::{self, AgentConfig, StartupWrites};

    /// A file whose owner turned SSH on and that holds no host key yet: the
    /// first SSH-enabled start.
    fn owner_turned_ssh_on(dir: &Path) -> PathBuf {
        let path = dir.join("config.toml");
        let mut cfg = config::test_fixture();
        cfg.ssh_enabled = true;
        cfg.ssh_host_key = None;
        config::save(&path, &cfg).unwrap();
        path
    }

    /// A start, up to its mint: the config it runs on, and its startup saves.
    fn start(path: &PathBuf) -> (AgentConfig, StartupWrites) {
        (config::load(path).unwrap(), StartupWrites::after_load(path))
    }

    /// `run_cmd`'s crash bookkeeping, a few steps after the mint: a startup
    /// save of the whole in-memory copy, which succeeds here.
    fn crash_bookkeeping(cfg: &mut AgentConfig, writes: &mut StartupWrites) {
        config::mark_run_starting(cfg);
        writes
            .save(cfg)
            .expect("the crash bookkeeping's save succeeds");
    }

    fn no_entropy() -> anyhow::Result<String> {
        Err(anyhow::anyhow!("no entropy"))
    }

    /// The start after a failed one tries the mint, once, and keeps the key it
    /// made.
    fn next_start_mints(path: &PathBuf) {
        let (mut cfg, mut writes) = start(path);
        let mut tries = 0;
        let mint = at_start(
            &mut cfg,
            || {
                tries += 1;
                crate::ssh::generate_host_key()
            },
            |c| writes.save(c),
        );
        assert_eq!(tries, 1, "the next start tries the mint again");
        assert_eq!(mint, Mint::Minted);
        let on_disk = config::load(path).unwrap();
        assert!(on_disk.ssh_enabled);
        assert!(
            crate::ssh::stored_host_key_is_usable(&on_disk),
            "and the key it made is in the file"
        );
    }

    /// The overlay's own device: carries nothing, and holds still.
    struct Wire;

    #[async_trait::async_trait]
    impl TunIo for Wire {
        async fn read_packet(&self) -> std::io::Result<Vec<u8>> {
            std::future::pending().await
        }

        async fn write_packet(&self, _packet: &[u8]) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// What a session needs from the daemon, on a consent folder of the
    /// test's own.
    fn services(dir: &Path) -> crate::ssh::SessionServices {
        crate::ssh::SessionServices {
            power_activity: None,
            asker: crate::ssh::Asker::Here {
                broker: crate::consent::ConsentBroker::new(
                    crate::consent::Mode::AutoGrant,
                    dir.join("consent"),
                )
                .expect("a consent folder"),
                indicator: crate::indicator::ViewerIndicator::disabled(),
                // A test never starts the real app.
                companion: crate::companion::Companion::Fixed(false),
            },
            activity: None,
        }
    }

    /// Whether this run's overlay device would answer SSH: the device the
    /// overlay's TUN factory hands out, after the SSH interception had its
    /// say, is not the one the overlay built.
    fn intercepts_ssh(cfg: &AgentConfig, dir: &Path) -> bool {
        let wire: Arc<dyn TunIo> = Arc::new(Wire);
        let built = wire.clone();
        let factory: TunFactory = Box::new(move |_, _, _| Ok(built.clone()));
        let factory = crate::ssh::maybe_intercept(factory, cfg, services(dir));
        let device = factory(
            Ipv4Addr::new(100, 64, 0, 7),
            Ipv4Addr::new(255, 192, 0, 0),
            1280,
        )
        .expect("the overlay's device");
        !Arc::ptr_eq(&device, &wire)
    }

    /// AC 1 and 3: the generator fails, and the crash bookkeeping saves the
    /// in-memory copy a few steps later. The owner's `true` stays in the file,
    /// and the next start mints.
    #[test]
    fn a_failed_generation_leaves_the_owners_ssh_enabled_in_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = owner_turned_ssh_on(dir.path());

        let (mut cfg, mut writes) = start(&path);
        let mint = at_start(&mut cfg, no_entropy, |c| writes.save(c));
        assert_eq!(mint, Mint::OffThisRun);
        crash_bookkeeping(&mut cfg, &mut writes);

        let on_disk = config::load(&path).unwrap();
        assert!(
            on_disk.ssh_enabled,
            "a failed mint turned the owner's ssh_enabled off in the file"
        );
        assert!(on_disk.ssh_host_key.is_none());
        next_start_mints(&path);
    }

    /// The field's case (an unreadable stored key) with a generator that fails
    /// as well: the unreadable key may go, as it always did, and the owner's
    /// `true` stays.
    #[test]
    fn an_unreadable_key_whose_replacement_fails_leaves_the_owners_ssh_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = owner_turned_ssh_on(dir.path());
        let mut owners = config::load(&path).unwrap();
        owners.ssh_host_key = Some("-----BEGIN OPENSSH PRIVATE KEY-----\\r\\nnot a key".into());
        config::save(&path, &owners).unwrap();

        let (mut cfg, mut writes) = start(&path);
        assert_eq!(
            at_start(&mut cfg, no_entropy, |c| writes.save(c)),
            Mint::OffThisRun
        );
        crash_bookkeeping(&mut cfg, &mut writes);

        let on_disk = config::load(&path).unwrap();
        assert!(
            on_disk.ssh_enabled,
            "a failed mint turned the owner's ssh_enabled off in the file"
        );
        assert!(
            !crate::ssh::stored_host_key_is_usable(&on_disk),
            "nothing usable was made"
        );
        next_start_mints(&path);
    }

    /// AC 2 and 3: the key is made but its save fails, and a later save (the
    /// crash bookkeeping's) succeeds. The owner's `true` stays in the file,
    /// the key that was never saved never reaches it, and the next start
    /// mints.
    #[test]
    fn a_failed_save_of_the_new_key_leaves_the_owners_ssh_enabled_in_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = owner_turned_ssh_on(dir.path());

        let (mut cfg, mut writes) = start(&path);
        let mint = at_start(&mut cfg, crate::ssh::generate_host_key, |_| {
            Err(anyhow::anyhow!("disk full"))
        });
        assert_eq!(mint, Mint::OffThisRun);
        assert!(
            cfg.ssh_host_key.is_none(),
            "a key that was not saved is never served"
        );
        crash_bookkeeping(&mut cfg, &mut writes);

        let on_disk = config::load(&path).unwrap();
        assert!(
            on_disk.ssh_enabled,
            "a failed mint turned the owner's ssh_enabled off in the file"
        );
        assert!(
            on_disk.ssh_host_key.is_none(),
            "nor does a later save write it"
        );
        next_start_mints(&path);
    }

    /// AC 4: the run whose mint failed serves no SSH. The interception is not
    /// installed, and the hello publishes no host public key.
    #[tokio::test]
    async fn a_failed_mint_serves_nothing_in_its_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = owner_turned_ssh_on(dir.path());
        let (mut cfg, mut writes) = start(&path);
        assert_eq!(
            at_start(&mut cfg, no_entropy, |c| writes.save(c)),
            Mint::OffThisRun
        );

        assert!(
            !intercepts_ssh(&cfg, dir.path()),
            "the interception is installed"
        );
        assert_eq!(crate::ssh::host_public_key(&cfg), "");
    }

    /// AC 6: a mint that works is unchanged. The key is in the file, the same
    /// one this run holds, and the run serves SSH with it.
    #[tokio::test]
    async fn a_successful_mint_persists_and_serves() {
        let dir = tempfile::tempdir().unwrap();
        let path = owner_turned_ssh_on(dir.path());
        let (mut cfg, mut writes) = start(&path);
        assert_eq!(
            at_start(&mut cfg, crate::ssh::generate_host_key, |c| writes.save(c)),
            Mint::Minted
        );
        crash_bookkeeping(&mut cfg, &mut writes);

        let on_disk = config::load(&path).unwrap();
        assert!(on_disk.ssh_enabled);
        assert!(crate::ssh::stored_host_key_is_usable(&on_disk));
        assert_eq!(on_disk.ssh_host_key, cfg.ssh_host_key, "the key it serves");
        assert!(intercepts_ssh(&cfg, dir.path()), "SSH is served");
        assert!(!crate::ssh::host_public_key(&cfg).is_empty());

        // And the start after it mints nothing.
        let (mut cfg, mut writes) = start(&path);
        assert_eq!(
            at_start(
                &mut cfg,
                || panic!("a usable key is never replaced"),
                |c| writes.save(c)
            ),
            Mint::NotNeeded
        );
    }

    /// A device whose owner keeps SSH off makes no key.
    #[test]
    fn ssh_off_makes_no_key() {
        let mut cfg = config::test_fixture();
        cfg.ssh_enabled = false;
        cfg.ssh_host_key = None;
        assert_eq!(
            at_start(
                &mut cfg,
                || panic!("SSH is off: no key is made"),
                |_| panic!("and nothing is saved")
            ),
            Mint::NotNeeded
        );
        assert!(cfg.ssh_host_key.is_none());
    }
}

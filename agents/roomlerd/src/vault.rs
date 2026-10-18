// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3-3 — the device's half of the vault: the key a leased value is
//! sealed to.
//!
//! The vault (`crates/modules/vault`) sends a value it leases to a session on
//! this device as a sealed box for a key only this daemon holds. That key is
//! made here: an X25519 key, made at the first hello that asks for it while
//! the device's own `vault_enabled` is on, kept in this process's memory and
//! written nowhere. Its public half rides the PRIMARY enrollment's hello
//! (`AgentHello::vault_key`), and the server stores it on every hello.
//!
//! Memory, not the config where the overlay and SSH keys live: those are
//! identities a peer pins, and a pin must outlive a restart. Nothing pins this
//! key. A leased value lives in daemon memory for its lease only, so a restart
//! ends every lease whatever becomes of the key, and a key kept on disk would
//! add one thing only: a way for whoever reads the config, or a backup of it,
//! to open a sealed value captured earlier. A new key at each start leaves
//! nothing on disk to read.
//!
//! ⚠️ Never the WireGuard key, and never on a secondary org's connection: that
//! org's server would learn a key another org's server holds too, which ties
//! the device's enrollments together (`docs/multi-org.md`).
//!
//! A build without the `vault` feature makes no key, its hello carries none,
//! and the vault leases it nothing.

/// The public half of this daemon's vault key for a hello, or the empty
/// string.
///
/// Empty unless the device's owner switched `vault_enabled` on, the
/// connection is the primary enrollment's, and this build has the `vault`
/// feature. The first call that passes makes the key; every later one gives
/// the same public half until the process ends.
pub fn hello_key(vault_enabled: bool, is_primary: bool) -> String {
    if !vault_enabled || !is_primary {
        return String::new();
    }
    public_key()
}

#[cfg(feature = "vault")]
static KEY: std::sync::OnceLock<crypto_box::SecretKey> = std::sync::OnceLock::new();

/// The process's key, made on first use.
#[cfg(feature = "vault")]
fn key() -> &'static crypto_box::SecretKey {
    KEY.get_or_init(|| {
        tracing::info!("vault: made this run's vault key (memory only)");
        crypto_box::SecretKey::generate(&mut crypto_box::aead::OsRng)
    })
}

#[cfg(feature = "vault")]
fn public_key() -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(key().public_key().as_bytes())
}

#[cfg(not(feature = "vault"))]
fn public_key() -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Off, or on a secondary org's connection, there is no key to give.
    #[test]
    fn no_key_while_off_or_on_a_secondary_connection() {
        assert_eq!(hello_key(false, true), "");
        assert_eq!(hello_key(false, false), "");
        assert_eq!(
            hello_key(true, false),
            "",
            "a secondary org never learns it"
        );
    }

    /// A build without the feature has no key to give, even switched on.
    #[cfg(not(feature = "vault"))]
    #[test]
    fn a_build_without_the_vault_has_no_key() {
        assert_eq!(hello_key(true, true), "");
    }

    /// On the primary connection: a key the server will store, and the same
    /// one at every hello this process says.
    #[cfg(feature = "vault")]
    #[test]
    fn the_primary_hello_carries_one_key_for_the_life_of_the_process() {
        use roomler_ai_remote_control::signaling::vault_key_is_well_formed;
        let first = hello_key(true, true);
        assert!(vault_key_is_well_formed(&first), "{first}");
        assert_eq!(
            hello_key(true, true),
            first,
            "a reconnect says the same key"
        );
    }

    /// What the vault seals to the published half, this process opens; a box
    /// for any other key it does not. Sealed here the way the server will,
    /// with the same crate.
    #[cfg(feature = "vault")]
    #[test]
    fn a_box_sealed_to_the_published_half_opens_here_and_no_other_does() {
        use base64::Engine;
        let rng = &mut crypto_box::aead::OsRng;
        let published = base64::engine::general_purpose::STANDARD
            .decode(hello_key(true, true))
            .unwrap();
        let to_us = crypto_box::PublicKey::from_slice(&published).unwrap();
        let sealed = to_us.seal(rng, b"canary-vault-value").unwrap();
        assert_eq!(key().unseal(&sealed).unwrap(), b"canary-vault-value");

        let other = crypto_box::SecretKey::generate(rng).public_key();
        let elsewhere = other.seal(rng, b"canary-vault-value").unwrap();
        assert!(key().unseal(&elsewhere).is_err());
        let mut tampered = sealed;
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(key().unseal(&tampered).is_err());
    }
}

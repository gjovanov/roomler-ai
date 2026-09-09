// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-52 gate 4 — the device's half of the cross-org access password.
//!
//! This is the module that makes the whole feature honest. Everything else in
//! FR-52 is policy the server can see and change; this is the one gate the
//! server cannot pass, cannot read and cannot forge, because the secret behind
//! it never leaves the machine and is never in a form the server could replay.
//!
//! # Why an aPAKE and not a hash
//!
//! The obvious build — the browser POSTs the password, the server compares a
//! stored hash — moves the gate into the server. A compromised control plane
//! then opens any device in the fleet and a database dump is a fleet-wide
//! credential dump. Every neighbouring subsystem (`exec_enabled`,
//! `ssh_enabled`, `remote_config_enabled`) is built so the device holds the
//! last refusal, and an outsider has no tenant membership behind them, so here
//! the password IS the whole authorization.
//!
//! The next design — the device stores `Argon2(password)` and the client sends
//! `HMAC(that, nonce)` — is much better and still wrong in two ways that
//! matter: the device would hold a **password-equivalent** secret, and the
//! relaying server would hold an **offline cracking oracle** (guess, derive,
//! HMAC over the nonce it chose, compare with the tag it saw). Against a
//! human-chosen password that is a real break, by exactly the party this
//! design exists to exclude.
//!
//! OPAQUE removes both. The device stores a registration record that is not
//! password-equivalent; a network observer — our own server, which relays
//! every byte — learns nothing it can attack offline; and the client
//! **authenticates the device** as well, so a compromised server cannot
//! impersonate the machine to harvest the operator's password.
//!
//! # What is stored, and why two values
//!
//! [`AgentConfig::external_access_setup`] (the `ServerSetup`) and
//! [`AgentConfig::external_access_verifier`] (the `ServerRegistration`).
//! OPAQUE needs both for every login, and neither is useful without the other.
//! They are written only by `rc password set`, and are deliberately absent
//! from the config *surface*: `config get` would print a credential and
//! `config set` would let anyone able to write the file choose the password,
//! which is the property gate 4 exists to deny.
//!
//! ⚠️ **The KSF is Argon2id, and that is load-bearing.** It is what makes a
//! STOLEN record expensive to attack offline. With `ksf::Identity` — the
//! choice opaque-ke's own doc example uses "only to ensure that the tests
//! execute quickly" — the record would be worth roughly what the password is,
//! and the "not password-equivalent" claim above would be false.
//!
//! # Registration runs entirely on this device
//!
//! OPAQUE registration is a client/server exchange, but the device plays both
//! halves: it is holding the password at that moment. Nothing crosses a
//! network, which is why the password can never be pushed from the dashboard —
//! a password typed into a web form has already crossed the server, and that
//! is the one thing this module exists to prevent. The dashboard may show
//! *set / not set* and may CLEAR; it can never set.

use base64::Engine as _;
use opaque_ke::{
    ClientRegistration, ClientRegistrationFinishParameters, Ristretto255, ServerRegistration,
    ServerSetup, TripleDh,
};
// ⚠️ NOT the workspace `rand`. opaque-ke is on rand 0.8 / rand_core 0.6 while
// the workspace is on 0.9, and the two `RngCore` traits are different types —
// the workspace `OsRng` fails to satisfy opaque-ke's bounds with an error that
// mentions `rand_core` only in a trailing note. See the Cargo.toml comment.
use rand_opaque::rngs::OsRng;

/// The ciphersuite, fixed for the life of the wire.
///
/// ⚠️ Every value in it is a compatibility surface: a device's stored record
/// is only readable by the same suite that wrote it, so changing any of these
/// invalidates every password already set in the field. A change here is a
/// forced re-set on every device, not a refactor.
pub struct Suite;

impl opaque_ke::CipherSuite for Suite {
    type OprfCs = Ristretto255;
    type KeyExchange = TripleDh<Ristretto255, sha2::Sha512>;
    /// ⚠️ NOT `ksf::Identity`. See the module docs — this is what stands
    /// between a stolen config file and the password in it.
    type Ksf = argon2::Argon2<'static>;
}

/// What `rc password set` persists. Both halves or neither: a config carrying
/// one without the other cannot authenticate anyone, and is the shape a
/// half-completed write would leave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    /// base64 `ServerSetup`
    pub setup: String,
    /// base64 `ServerRegistration`
    pub verifier: String,
}

/// Anything that can go wrong turning a password into a stored credential.
///
/// Deliberately does NOT carry the password, an OPAQUE internal, or any part
/// of either — an error type is the most likely thing to end up in a log line.
#[derive(Debug)]
pub enum Error {
    /// The password was empty. Refused rather than registered, because an
    /// empty password that "works" is a device with no gate 4 at all.
    EmptyPassword,
    /// The OPAQUE protocol refused. The cause is not surfaced on purpose.
    Protocol,
    /// A stored value is not valid base64, or not a value this suite wrote.
    Corrupt,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::EmptyPassword => "the external-access password must not be empty",
            Self::Protocol => "could not derive the external-access credential",
            Self::Corrupt => {
                "the stored external-access credential is unreadable — set the password again"
            }
        })
    }
}

impl std::error::Error for Error {}

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// The OPAQUE identity a record is bound to.
///
/// Fixed rather than the device id: the record lives in this device's own
/// config and is never shared, so there is nothing for an identity to
/// disambiguate — and binding to a device id would invalidate every stored
/// password the first time a device is re-enrolled and gets a new one.
const IDENTITY: &[u8] = b"roomler-external-access";

/// Turn a password into the pair this device will store.
///
/// Runs both halves of OPAQUE registration locally. The password is used and
/// dropped here; nothing derived from it that could stand in for it is
/// returned.
pub fn register(password: &str) -> Result<Credential, Error> {
    if password.is_empty() {
        return Err(Error::EmptyPassword);
    }
    let mut rng = OsRng;
    let setup = ServerSetup::<Suite>::new(&mut rng);
    let verifier = register_with(&setup, password, &mut rng)?;
    Ok(Credential {
        setup: b64().encode(setup.serialize()),
        verifier: b64().encode(verifier.serialize()),
    })
}

/// Re-register a password against an EXISTING `ServerSetup`.
///
/// Split out because a password *change* must keep the setup: it is
/// long-lived per device, and minting a fresh one on every change would be a
/// silent extra way for the two stored halves to stop matching.
fn register_with(
    setup: &ServerSetup<Suite>,
    password: &str,
    rng: &mut OsRng,
) -> Result<ServerRegistration<Suite>, Error> {
    let pw = password.as_bytes();
    let client_start = ClientRegistration::<Suite>::start(rng, pw).map_err(|_| Error::Protocol)?;
    let server_start = ServerRegistration::<Suite>::start(setup, client_start.message, IDENTITY)
        .map_err(|_| Error::Protocol)?;
    let client_finish = client_start
        .state
        .finish(
            rng,
            pw,
            server_start.message,
            ClientRegistrationFinishParameters::default(),
        )
        .map_err(|_| Error::Protocol)?;
    Ok(ServerRegistration::<Suite>::finish(client_finish.message))
}

/// Read a stored credential back into the OPAQUE types a login needs.
///
/// ⚠️ `Corrupt` and "no password set" are different answers and the caller
/// must keep them apart: the first is a device whose owner needs to set the
/// password again, the second is a device that never had one. Collapsing them
/// would tell an operator to redo work they never did.
pub fn parse(cred: &Credential) -> Result<(ServerSetup<Suite>, ServerRegistration<Suite>), Error> {
    let setup_bytes = b64().decode(&cred.setup).map_err(|_| Error::Corrupt)?;
    let verifier_bytes = b64().decode(&cred.verifier).map_err(|_| Error::Corrupt)?;
    let setup = ServerSetup::<Suite>::deserialize(&setup_bytes).map_err(|_| Error::Corrupt)?;
    let verifier =
        ServerRegistration::<Suite>::deserialize(&verifier_bytes).map_err(|_| Error::Corrupt)?;
    Ok((setup, verifier))
}

#[cfg(test)]
mod tests {
    use super::*;
    use opaque_ke::{ClientLogin, ClientLoginFinishParameters, ServerLogin, ServerLoginParameters};

    /// Test passwords are BUILT, never written as literals.
    ///
    /// ⚠️ Not cosmetic: a secret scanner cannot tell a KDF input in a unit
    /// test from a leaked credential, and this file failed GitGuardian on a
    /// literal the moment it was pushed. Nothing here needs to LOOK like a
    /// password — what the tests exercise is that two inputs differ.
    fn pw(tag: &str) -> String {
        format!("fr52-not-a-credential-{tag}")
    }

    /// Drive a full OPAQUE login against a record this module registered.
    ///
    /// This is the point of the test module. Registration on its own is
    /// unfalsifiable — it always produces *something* — and the wire that
    /// would exercise it is FR-52 P3. Running the login here proves the stored
    /// pair is actually usable, and proves it now rather than after a protocol
    /// change has quietly made every fielded password unreadable.
    fn login(cred: &Credential, password: &str) -> Result<(), ()> {
        let (setup, verifier) = parse(cred).map_err(|_| ())?;
        let mut rng = OsRng;
        let c1 = ClientLogin::<Suite>::start(&mut rng, password.as_bytes()).map_err(|_| ())?;
        let s1 = ServerLogin::<Suite>::start(
            &mut rng,
            &setup,
            Some(verifier),
            c1.message,
            IDENTITY,
            ServerLoginParameters::default(),
        )
        .map_err(|_| ())?;
        let c2 = c1
            .state
            .finish(
                &mut rng,
                password.as_bytes(),
                s1.message,
                ClientLoginFinishParameters::default(),
            )
            .map_err(|_| ())?;
        let s2 = s1
            .state
            .finish(c2.message, ServerLoginParameters::default())
            .map_err(|_| ())?;
        // Both sides derived a session key, and they AGREE. That agreement is
        // the thing P3 binds the DTLS fingerprint to; without it a "verified"
        // password would still leave the server free to substitute its own
        // peer, and the whole aPAKE would buy less than it looks like.
        assert_eq!(
            c2.session_key, s2.session_key,
            "client and server derived different session keys"
        );
        Ok(())
    }

    #[test]
    fn a_registered_password_authenticates() {
        let cred = register(&pw("alpha")).expect("registration");
        login(&cred, &pw("alpha")).expect("the right password must authenticate");
    }

    /// The half that makes the test above mean anything. A round trip that
    /// only ever tries the right password passes on an implementation that
    /// accepts everything.
    #[test]
    fn a_wrong_password_does_not() {
        let cred = register(&pw("alpha")).unwrap();
        assert!(
            login(&cred, &(pw("alpha") + "x")).is_err(),
            "a one-character-off password authenticated — gate 4 is not a gate"
        );
        assert!(login(&cred, "").is_err());
    }

    /// Two devices setting the SAME password must not produce the same record:
    /// the setup is per-device, so a record lifted from one config cannot be
    /// dropped into another, and two devices are not correlatable by their
    /// stored verifier.
    #[test]
    fn the_same_password_registers_differently_on_two_devices() {
        let a = register(&pw("shared")).unwrap();
        let b = register(&pw("shared")).unwrap();
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.setup, b.setup);
        // And a record is bound to ITS OWN setup: crossing them must fail.
        let crossed = Credential {
            setup: a.setup.clone(),
            verifier: b.verifier.clone(),
        };
        assert!(
            login(&crossed, &pw("shared")).is_err(),
            "a verifier authenticated against another device's setup"
        );
    }

    /// A password change keeps the setup — see [`register_with`]. Losing the
    /// setup on every change is a silent way for the two stored halves to
    /// drift apart.
    #[test]
    fn changing_the_password_keeps_the_setup_and_invalidates_the_old_one() {
        let first = register(&pw("first")).unwrap();
        let setup_bytes = b64().decode(&first.setup).unwrap();
        let setup = ServerSetup::<Suite>::deserialize(&setup_bytes).unwrap();
        let mut rng = OsRng;
        let changed = Credential {
            setup: first.setup.clone(),
            verifier: b64().encode(
                register_with(&setup, &pw("second"), &mut rng)
                    .unwrap()
                    .serialize(),
            ),
        };
        login(&changed, &pw("second")).expect("the new password authenticates");
        assert!(
            login(&changed, &pw("first")).is_err(),
            "the old password still works after a change"
        );
    }

    /// An empty password is REFUSED, not registered. A device whose password
    /// is "" has no gate 4 while looking exactly like one that does.
    #[test]
    fn an_empty_password_is_refused() {
        assert!(matches!(register(""), Err(Error::EmptyPassword)));
    }

    /// Unreadable is its own answer, distinct from absent. An operator told
    /// "set it again" for a device that never had one is being sent to redo
    /// work they never did.
    #[test]
    fn a_corrupt_credential_says_so_rather_than_panicking() {
        let cred = Credential {
            setup: "not base64!!".into(),
            verifier: "also not".into(),
        };
        assert!(matches!(parse(&cred), Err(Error::Corrupt)));

        let good = register(&pw("corrupt")).unwrap();
        let truncated = Credential {
            setup: good.setup[..good.setup.len() / 2].to_string(),
            verifier: good.verifier.clone(),
        };
        assert!(matches!(parse(&truncated), Err(Error::Corrupt)));
    }

    /// ⚠️ The error type is the most likely thing to reach a log line, so it
    /// must not carry the password or anything derived from it.
    #[test]
    fn errors_never_quote_the_password() {
        let marker = "fr52-marker-not-a-credential";
        let e = register("").unwrap_err();
        assert!(!format!("{e}").contains(marker));
        assert!(!format!("{e:?}").contains(marker));
        let bad = Credential {
            setup: marker.into(),
            verifier: marker.into(),
        };
        let e = parse(&bad).unwrap_err();
        assert!(!format!("{e}").contains(marker));
        assert!(!format!("{e:?}").contains(marker));
    }
}

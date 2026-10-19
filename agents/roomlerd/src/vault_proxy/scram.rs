// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P3-5b — SCRAM-SHA-256 (RFC 5802, RFC 7677), the client's half: how
//! a vault proxy proves to an upstream that it holds a password without ever
//! sending it, and checks that the upstream held it too.
//!
//! ⚠️ The password never crosses the wire in any form a server could replay
//! elsewhere: the client sends a proof bound to this exchange's two nonces,
//! and trusts the session only once the server's own signature checks. A
//! server that cannot sign never had the password, and is refused.
//!
//! No channel binding yet (`n,,`): the proxy speaks SCRAM only inside TLS
//! whose certificate it verified, so the exchange runs with an upstream the
//! system trusts for that name. What binding adds is catching a TLS
//! terminator the system also trusts (an inspection proxy whose CA is in
//! the OS store) relaying the exchange: P3-5b-3. A server that offers
//! `-PLUS` accepts a client that does not bind (RFC 5802 §6: `n` is "the
//! client does not support it", never a downgrade the server must refuse).

use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

type HmacSha256 = Hmac<Sha256>;
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// The most PBKDF2 iterations an upstream may ask for: past it, an exchange
/// is a way to make the daemon burn CPU. (Postgres and MongoDB default to
/// 4096 and 15000.)
pub(crate) const MAX_ITERATIONS: u32 = 10_000_000;

/// The GS2 header: no channel binding, no authorization identity.
const GS2_HEADER: &str = "n,,";

/// One exchange, from the client's first message to the server's signature.
pub(crate) struct ScramClient {
    /// SASLprepped (RFC 4013), as both ends derive the key from it.
    password: Zeroizing<String>,
    client_nonce: String,
    client_first_bare: String,
    /// Set by [`Self::client_final`]: what the server must sign with.
    server_signature: Option<[u8; 32]>,
}

impl ScramClient {
    /// Begin an exchange for `user` (Postgres sends none: it takes the user
    /// from its startup message) and `password`, with a fresh random nonce.
    /// Err for a password SASLprep refuses.
    pub(crate) fn new(user: &str, password: &str) -> Result<Self, String> {
        Self::with_nonce(user, password, &B64.encode(rand::random::<[u8; 18]>()))
    }

    fn with_nonce(user: &str, password: &str, nonce: &str) -> Result<Self, String> {
        let password = stringprep::saslprep(password)
            .map_err(|_| "the password is not one SCRAM can use (SASLprep refused it)")?;
        let client_first_bare = format!("n={},r={nonce}", escape(user));
        Ok(Self {
            password: Zeroizing::new(password.into_owned()),
            client_nonce: nonce.to_string(),
            client_first_bare,
            server_signature: None,
        })
    }

    /// `client-first-message`.
    pub(crate) fn client_first(&self) -> String {
        format!("{GS2_HEADER}{}", self.client_first_bare)
    }

    /// Read the `server-first-message` and answer the `client-final-message`,
    /// with the proof. Err for a server whose nonce is not this exchange's,
    /// whose salt does not decode, or whose iteration count is out of
    /// bounds.
    pub(crate) fn client_final(&mut self, server_first: &str) -> Result<String, String> {
        let mut nonce = None;
        let mut salt = None;
        let mut iterations = None;
        for attr in server_first.split(',') {
            match attr.split_once('=') {
                Some(("r", v)) => nonce = Some(v),
                Some(("s", v)) => salt = Some(v),
                Some(("i", v)) => iterations = Some(v),
                // A mandatory extension this client does not know (RFC
                // 5802 §5.1): refused, as the RFC requires.
                Some(("m", _)) => return Err("the server asked for a SCRAM extension".into()),
                _ => {}
            }
        }
        let nonce = nonce.ok_or("the server's first message has no nonce")?;
        if !nonce.starts_with(&self.client_nonce) || nonce.len() <= self.client_nonce.len() {
            return Err("the server's nonce is not this exchange's".into());
        }
        let salt = B64
            .decode(salt.ok_or("the server's first message has no salt")?)
            .map_err(|_| "the server's salt does not decode")?;
        let iterations: u32 = iterations
            .ok_or("the server's first message has no iteration count")?
            .parse()
            .map_err(|_| "the server's iteration count is not a number")?;
        if iterations == 0 || iterations > MAX_ITERATIONS {
            return Err(format!(
                "the server asked for {iterations} iterations (at most {MAX_ITERATIONS})"
            ));
        }

        let mut salted = Zeroizing::new([0u8; 32]);
        pbkdf2::pbkdf2_hmac::<Sha256>(self.password.as_bytes(), &salt, iterations, salted.as_mut());
        let client_key = Zeroizing::new(hmac(salted.as_ref(), b"Client Key"));
        let stored_key: [u8; 32] = Sha256::digest(client_key.as_ref()).into();
        let without_proof = format!("c={},r={nonce}", B64.encode(GS2_HEADER));
        let auth_message = format!("{},{server_first},{without_proof}", self.client_first_bare);
        let signature = hmac(&stored_key, auth_message.as_bytes());
        let mut proof = [0u8; 32];
        for (p, (k, s)) in proof
            .iter_mut()
            .zip(client_key.iter().zip(signature.iter()))
        {
            *p = k ^ s;
        }
        let server_key = Zeroizing::new(hmac(salted.as_ref(), b"Server Key"));
        self.server_signature = Some(hmac(server_key.as_ref(), auth_message.as_bytes()));
        Ok(format!("{without_proof},p={}", B64.encode(proof)))
    }

    /// Check the `server-final-message`: Ok only for the signature only a
    /// server that holds the password could make. An `e=` is the server's
    /// own refusal, said in its words.
    pub(crate) fn verify_server_final(&self, server_final: &str) -> Result<(), String> {
        let expected = self
            .server_signature
            .ok_or("the server's final message came before its first")?;
        for attr in server_final.split(',') {
            match attr.split_once('=') {
                Some(("e", why)) => return Err(format!("the server refused: {why}")),
                Some(("v", sig)) => {
                    let sig = B64
                        .decode(sig)
                        .map_err(|_| "the server's signature does not decode")?;
                    // Compared in constant time: an exchange that leaks
                    // nothing is simpler to reason about than one that leaks
                    // little.
                    let same = sig.len() == expected.len()
                        && sig
                            .iter()
                            .zip(expected.iter())
                            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                            == 0;
                    return if same {
                        Ok(())
                    } else {
                        Err("the server's signature is wrong: it does not hold the password".into())
                    };
                }
                _ => {}
            }
        }
        Err("the server's final message has no signature".into())
    }
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// A SCRAM `saslname`: `=` and `,` escaped (RFC 5802 §5.1).
fn escape(name: &str) -> String {
    name.replace('=', "=3D").replace(',', "=2C")
}

/// The server's half, for the proxies' tests: what an upstream checks and
/// answers, from the same RFC.
#[cfg(test)]
pub(crate) mod server {
    use super::*;

    /// One server-side exchange for `password`, with `salt` and
    /// `iterations`.
    pub(crate) struct ScramServer {
        salted: [u8; 32],
        salt: Vec<u8>,
        iterations: u32,
        nonce: String,
        client_first_bare: String,
        server_first: String,
    }

    impl ScramServer {
        pub(crate) fn new(password: &str, salt: &[u8], iterations: u32, nonce: &str) -> Self {
            let password = stringprep::saslprep(password).unwrap();
            let mut salted = [0u8; 32];
            pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, iterations, &mut salted);
            Self {
                salted,
                salt: salt.to_vec(),
                iterations,
                nonce: nonce.to_string(),
                client_first_bare: String::new(),
                server_first: String::new(),
            }
        }

        /// The `server-first-message` for a `client-first-message`.
        pub(crate) fn server_first(&mut self, client_first: &str) -> String {
            let bare = client_first
                .strip_prefix(GS2_HEADER)
                .expect("a client that does not bind");
            let client_nonce = bare.split(',').find_map(|a| a.strip_prefix("r=")).unwrap();
            self.client_first_bare = bare.to_string();
            self.server_first = format!(
                "r={client_nonce}{},s={},i={}",
                self.nonce,
                B64.encode(&self.salt),
                self.iterations
            );
            self.server_first.clone()
        }

        /// The `server-final-message` for a `client-final-message`: its
        /// signature when the proof checks, `e=invalid-proof` when it does
        /// not.
        pub(crate) fn server_final(&self, client_final: &str) -> Result<String, String> {
            let (without_proof, proof) = client_final.rsplit_once(",p=").unwrap();
            let auth_message = format!(
                "{},{},{without_proof}",
                self.client_first_bare, self.server_first
            );
            let client_key = hmac(&self.salted, b"Client Key");
            let stored_key: [u8; 32] = Sha256::digest(client_key).into();
            let signature = hmac(&stored_key, auth_message.as_bytes());
            let proof = B64.decode(proof).unwrap();
            let recovered: Vec<u8> = proof
                .iter()
                .zip(signature.iter())
                .map(|(p, s)| p ^ s)
                .collect();
            if Sha256::digest(&recovered).as_slice() != stored_key {
                return Err("e=invalid-proof".into());
            }
            let server_key = hmac(&self.salted, b"Server Key");
            Ok(format!(
                "v={}",
                B64.encode(hmac(&server_key, auth_message.as_bytes()))
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::server::ScramServer;
    use super::*;

    /// RFC 7677 §3's exchange, byte for byte: the proof the client sends and
    /// the signature it accepts.
    #[test]
    fn the_rfc_7677_exchange_comes_out_byte_for_byte() {
        let mut c = ScramClient::with_nonce("user", "pencil", "rOprNGfwEbeRWgbNEkqO").unwrap();
        assert_eq!(c.client_first(), "n,,n=user,r=rOprNGfwEbeRWgbNEkqO");
        let server_first = "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,\
                            s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";
        assert_eq!(
            c.client_final(server_first).unwrap(),
            "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,\
             p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="
        );
        c.verify_server_final("v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=")
            .unwrap();
    }

    /// Against the test server: a matching password is let in both ways; a
    /// wrong one is refused by the server, and a server that does not hold
    /// the password is refused by the client.
    #[test]
    fn only_the_same_password_on_both_sides_completes() {
        let exchange = |client_pw: &str, server_pw: &str| {
            let mut c = ScramClient::new("", client_pw).unwrap();
            let mut s = ScramServer::new(server_pw, b"salt-for-a-test", 4096, "srvnonce");
            let first = s.server_first(&c.client_first());
            let fin = c.client_final(&first).unwrap();
            s.server_final(&fin).map(|v| c.verify_server_final(&v))
        };
        assert_eq!(exchange("same", "same"), Ok(Ok(())));
        assert_eq!(
            exchange("mine", "theirs"),
            Err("e=invalid-proof".into()),
            "the server refuses a wrong proof"
        );
        // A server that answers without the password: its signature cannot
        // check.
        let mut c = ScramClient::new("", "real").unwrap();
        let mut s = ScramServer::new("forged", b"salt", 4096, "n");
        let first = s.server_first(&c.client_first());
        c.client_final(&first).unwrap();
        let forged = "v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=";
        assert!(c.verify_server_final(forged).is_err());
        assert_eq!(
            c.verify_server_final("e=other-error"),
            Err("the server refused: other-error".into())
        );
    }

    /// A server first message is checked before any work is done for it.
    #[test]
    fn a_server_first_message_is_held_to_this_exchange_and_its_bounds() {
        let mut c = ScramClient::with_nonce("", "pw", "abc").unwrap();
        for bad in [
            // Another exchange's nonce, or the client's echoed alone.
            "r=xyzdef,s=c2FsdA==,i=4096",
            "r=abc,s=c2FsdA==,i=4096",
            "s=c2FsdA==,i=4096",
            "r=abcdef,s=***,i=4096",
            "r=abcdef,i=4096",
            "r=abcdef,s=c2FsdA==",
            "r=abcdef,s=c2FsdA==,i=0",
            "r=abcdef,s=c2FsdA==,i=10000001",
            "r=abcdef,s=c2FsdA==,i=many",
            "m=ext,r=abcdef,s=c2FsdA==,i=4096",
        ] {
            assert!(c.client_final(bad).is_err(), "{bad}");
        }
        assert!(c.client_final("r=abcdef,s=c2FsdA==,i=4096").is_ok());
        assert!(
            ScramClient::new("", "pw")
                .unwrap()
                .verify_server_final("v=AAAA")
                .is_err(),
            "a final message before a first"
        );
    }

    /// The user is a `saslname` (RFC 5802 §5.1), and the password SASLprepped
    /// (RFC 4013) as the server derives it: a non-ASCII space is a space.
    #[test]
    fn names_are_escaped_and_passwords_saslprepped() {
        let c = ScramClient::with_nonce("a=b,c", "pw", "n").unwrap();
        assert_eq!(c.client_first(), "n,,n=a=3Db=2Cc,r=n");
        let exchange = |client_pw: &str, server_pw: &str| {
            let mut c = ScramClient::new("", client_pw).unwrap();
            let mut s = ScramServer::new(server_pw, b"salt", 4096, "srv");
            let first = s.server_first(&c.client_first());
            let fin = c.client_final(&first).unwrap();
            s.server_final(&fin).is_ok()
        };
        assert!(exchange("a\u{00A0}b", "a b"), "a no-break space is a space");
        assert!(
            ScramClient::new("", "bad\u{0007}").is_err(),
            "a control character is refused"
        );
    }
}

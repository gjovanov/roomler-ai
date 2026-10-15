// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-102 P5 (D6) — verifying SendGrid's signed Event Webhook.
//!
//! SendGrid signs `timestamp || body` with ECDSA over P-256 and SHA-256: the
//! base64 DER signature and the timestamp travel in two headers, and the
//! verification key is the base64 DER `SubjectPublicKeyInfo` SendGrid shows
//! (Mail Settings → Event Webhook → Signature Verification). Here, beside
//! the Stripe signature check, so the route (the `saas` module's) and the
//! host's views — which must know whether delivery CAN be measured — agree
//! on whether a configured key is usable.

use base64::Engine;
use ring::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};

/// The DER prefix of every P-256 `SubjectPublicKeyInfo` with an
/// uncompressed point: SEQUENCE { SEQUENCE { id-ecPublicKey, prime256v1 },
/// BIT STRING (66 bytes, no unused bits) } — the 65-byte point follows.
/// DER is canonical, so a P-256 key in this form has exactly these bytes
/// (SendGrid's keys start `MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE`).
pub const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];
const P256_POINT_LEN: usize = 65;

/// SendGrid's verification key, parsed once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventVerifier {
    /// The uncompressed point (`04 || X || Y`).
    point: Vec<u8>,
}

impl EventVerifier {
    /// The key as SendGrid shows it — base64 of the DER
    /// `SubjectPublicKeyInfo` — or the same key in PEM armour. Anything that
    /// is not an uncompressed P-256 key is refused: a key the verifier
    /// cannot use must not become a webhook that silently accepts nothing.
    pub fn from_config(raw: &str) -> Result<Self, String> {
        let b64: String = raw
            .lines()
            .filter(|l| !l.trim_start().starts_with("-----"))
            .flat_map(|l| l.chars())
            .filter(|c| !c.is_whitespace())
            .collect();
        if b64.is_empty() {
            return Err("empty".into());
        }
        let der = base64::engine::general_purpose::STANDARD
            .decode(b64.as_bytes())
            .map_err(|_| "not base64".to_string())?;
        if der.len() != P256_SPKI_PREFIX.len() + P256_POINT_LEN
            || der[..P256_SPKI_PREFIX.len()] != P256_SPKI_PREFIX
            || der[P256_SPKI_PREFIX.len()] != 0x04
        {
            return Err("not an uncompressed P-256 public key (SendGrid's ECDSA key)".into());
        }
        Ok(Self {
            point: der[P256_SPKI_PREFIX.len()..].to_vec(),
        })
    }

    /// Whether `signature_b64` (base64 of an ASN.1 DER ECDSA signature)
    /// signs `timestamp || body` under this key. Any malformed input is a
    /// plain `false`. Either of a signature's two valid `s` values verifies
    /// (SendGrid's signer does not normalise it).
    pub fn verify(&self, timestamp: &str, body: &[u8], signature_b64: &str) -> bool {
        let Ok(sig) = base64::engine::general_purpose::STANDARD.decode(signature_b64.trim()) else {
            return false;
        };
        let mut signed = Vec::with_capacity(timestamp.len() + body.len());
        signed.extend_from_slice(timestamp.as_bytes());
        signed.extend_from_slice(body);
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, &self.point)
            .verify(&signed, &sig)
            .is_ok()
    }
}

/// Whether `newsletter.event_webhook_public_key` makes the webhook work:
/// set, and a key the verifier can use. What the host's views read to tell
/// "no event arrived" (a measured zero) from "no event can arrive".
pub fn webhook_key_usable(raw: &str) -> bool {
    !raw.trim().is_empty() && EventVerifier::from_config(raw).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::{
        rand::SystemRandom,
        signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair},
    };

    /// A fresh key pair and the key as SendGrid would show it.
    fn keypair() -> (EcdsaKeyPair, String) {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        let mut spki = P256_SPKI_PREFIX.to_vec();
        spki.extend_from_slice(pair.public_key().as_ref());
        (pair, base64::engine::general_purpose::STANDARD.encode(spki))
    }

    fn sign(pair: &EcdsaKeyPair, ts: &str, body: &[u8]) -> String {
        let mut msg = ts.as_bytes().to_vec();
        msg.extend_from_slice(body);
        let sig = pair.sign(&SystemRandom::new(), &msg).unwrap();
        base64::engine::general_purpose::STANDARD.encode(sig.as_ref())
    }

    /// The prefix is the start of the key SendGrid's UI shows.
    #[test]
    fn the_spki_prefix_is_sendgrids() {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode("MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE")
            .unwrap();
        assert_eq!(&decoded[..26], &P256_SPKI_PREFIX[..]);
        assert_eq!(decoded[26], 0x04);
    }

    #[test]
    fn a_signature_by_the_key_verifies_and_nothing_else_does() {
        let (pair, b64) = keypair();
        let v = EventVerifier::from_config(&b64).unwrap();
        assert!(webhook_key_usable(&b64));
        let body = br#"[{"event":"delivered","email":"a@x.io","nl_issue":"oct"}]"#;
        let ts = "1760515200";
        let sig = sign(&pair, ts, body);
        assert!(v.verify(ts, body, &sig));
        // A different body, a different timestamp, a different key, junk.
        assert!(!v.verify(ts, br#"[{"event":"bounce"}]"#, &sig));
        assert!(!v.verify("1760515201", body, &sig));
        let (other, _) = keypair();
        assert!(!v.verify(ts, body, &sign(&other, ts, body)));
        assert!(!v.verify(ts, body, "not base64!"));
        assert!(!v.verify(ts, body, ""));
        // PEM armour is the same key.
        let pem = format!(
            "-----BEGIN PUBLIC KEY-----\n{}\n{}\n-----END PUBLIC KEY-----\n",
            &b64[..64],
            &b64[64..]
        );
        assert_eq!(EventVerifier::from_config(&pem).unwrap(), v);
    }

    /// ECDSA signatures are not unique: SendGrid's signer does not
    /// normalise `s`, so the "high-S" twin of a valid signature must verify
    /// too, or a share of genuine batches would be refused at random.
    #[test]
    fn a_high_s_signature_verifies() {
        let (pair, b64) = keypair();
        let v = EventVerifier::from_config(&b64).unwrap();
        let (body, ts) = (b"[]", "1760515200");
        let der = base64::engine::general_purpose::STANDARD
            .decode(sign(&pair, ts, body))
            .unwrap();
        let flipped = flip_s(&der);
        assert_ne!(flipped, der);
        let flipped = base64::engine::general_purpose::STANDARD.encode(flipped);
        assert!(v.verify(ts, body, &flipped), "s and n − s are both valid");
    }

    /// `SEQUENCE { INTEGER r, INTEGER s }` with `s` replaced by `n − s`.
    fn flip_s(der: &[u8]) -> Vec<u8> {
        // The P-256 group order n.
        const N: [u8; 32] = [
            0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xbc, 0xe6, 0xfa, 0xad, 0xa7, 0x17, 0x9e, 0x84, 0xf3, 0xb9, 0xca, 0xc2,
            0xfc, 0x63, 0x25, 0x51,
        ];
        let int_at = |i: usize| -> (usize, usize) {
            assert_eq!(der[i], 0x02);
            (i + 2, der[i + 1] as usize)
        };
        let (r_start, r_len) = int_at(2);
        let (s_start, s_len) = int_at(r_start + r_len);
        let r = &der[r_start..r_start + r_len];
        let raw_s = &der[s_start..s_start + s_len];
        let raw_s = &raw_s[raw_s.len().saturating_sub(32)..];
        let mut s = [0u8; 32];
        s[32 - raw_s.len()..].copy_from_slice(raw_s);
        // n − s, big-endian.
        let mut out = [0u8; 32];
        let mut borrow = 0i16;
        for i in (0..32).rev() {
            let mut d = N[i] as i16 - s[i] as i16 - borrow;
            borrow = 0;
            if d < 0 {
                d += 256;
                borrow = 1;
            }
            out[i] = d as u8;
        }
        let encode_int = |v: &[u8]| -> Vec<u8> {
            let mut v: Vec<u8> = v.iter().copied().skip_while(|b| *b == 0).collect();
            if v.first().is_some_and(|b| b & 0x80 != 0) {
                v.insert(0, 0);
            }
            let mut e = vec![0x02, v.len() as u8];
            e.extend(v);
            e
        };
        let mut body = encode_int(r);
        body.extend(encode_int(&out));
        let mut seq = vec![0x30, body.len() as u8];
        seq.extend(body);
        seq
    }

    #[test]
    fn a_key_that_is_not_p256_is_refused() {
        for bad in ["", "   ", "not base64 at all!"] {
            assert!(EventVerifier::from_config(bad).is_err(), "{bad:?}");
            assert!(!webhook_key_usable(bad));
        }
        // Valid base64 that is not the P-256 SPKI shape.
        let junk = base64::engine::general_purpose::STANDARD.encode([0u8; 91]);
        assert!(EventVerifier::from_config(&junk).is_err());
        assert!(!webhook_key_usable(&junk));
        let (_, b64) = keypair();
        let mut der = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .unwrap();
        der[26] = 0x02; // a compressed-point marker
        let bad = base64::engine::general_purpose::STANDARD.encode(der);
        assert!(EventVerifier::from_config(&bad).is_err());
    }
}

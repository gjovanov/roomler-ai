//! DERP relay tickets — EdDSA-signed admission for the standalone regional
//! relays (`crates/derp-relay`).
//!
//! Regional PoPs are cheap VPSes and must hold MINIMAL secrets: never the JWT
//! signing secret (symmetric — holding it means forging any user/agent token).
//! Instead the API mints a short, narrowly-scoped ticket with an Ed25519
//! PRIVATE key, and each relay verifies with the PUBLIC key only. A ticket
//! binds `{network, wg pubkey}`, so a relay can enforce exactly the two
//! invariants the central `/derp` enforces from Mongo — register-your-own-key
//! and same-network forwarding — without any database access.
//!
//! Key format: `ROOMLER__RELAY__DERP_TICKET_PRIVATE_KEY` = base64(PKCS#8 DER)
//! (`openssl genpkey -algorithm ed25519 -outform DER | base64 -w0`). The
//! relay's `DERP_TICKET_PUBLIC_KEY` = base64(raw 32-byte public key) — logged
//! by the API at startup for copy-paste.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};

/// Re-exported for `crates/derp-relay`, so the relay binary shares this exact
/// jsonwebtoken (no version drift between mint and verify).
pub use jsonwebtoken;
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Ticket lifetime. Long — admission is checked at accept-time only (exactly
/// like the central `/derp`'s JWT check), so expiry mid-connection is a
/// non-event; agents refresh at ~90 % anyway.
pub const DERP_TICKET_TTL: Duration = Duration::from_secs(24 * 3600);
/// Clock-skew tolerance on verification (cheap VPSes drift).
pub const DERP_TICKET_LEEWAY_SECS: u64 = 300;
const DERP_AUDIENCE: &str = "derp";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DerpTicketClaims {
    /// Always `"derp"` — never interchangeable with user/agent JWTs.
    pub aud: String,
    /// Overlay network id (hex) — the relay's forwarding scope.
    pub net: String,
    /// base64 WG public key — the ONLY key this ticket may register.
    pub pk: String,
    pub exp: u64,
}

/// The API-side minting half. Built from the configured private key; derives
/// and exposes the public half so operators can copy it to PoPs.
pub struct DerpTicketSigner {
    enc: EncodingKey,
    public_b64: String,
}

impl DerpTicketSigner {
    /// Load from base64(PKCS#8 DER). Errors on undecodable/non-Ed25519 input.
    pub fn from_pkcs8_b64(private_b64: &str) -> Result<Self, String> {
        let der = B64
            .decode(private_b64.trim())
            .map_err(|e| format!("derp ticket key: invalid base64: {e}"))?;
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8_maybe_unchecked(&der)
            .map_err(|e| format!("derp ticket key: not an Ed25519 PKCS#8 key: {e}"))?;
        use ring::signature::KeyPair as _;
        let public_b64 = B64.encode(pair.public_key().as_ref());
        Ok(Self {
            enc: EncodingKey::from_ed_der(&der),
            public_b64,
        })
    }

    /// base64(raw 32-byte Ed25519 public key) — what a relay's
    /// `DERP_TICKET_PUBLIC_KEY` must be set to.
    pub fn public_key_b64(&self) -> &str {
        &self.public_b64
    }

    /// Mint a ticket for `(network hex, wg pubkey b64)`. Returns
    /// `(token, exp_unix)`.
    pub fn mint(&self, network_hex: &str, wg_pubkey_b64: &str) -> Result<(String, u64), String> {
        let exp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            + DERP_TICKET_TTL.as_secs();
        let claims = DerpTicketClaims {
            aud: DERP_AUDIENCE.to_string(),
            net: network_hex.to_string(),
            pk: wg_pubkey_b64.to_string(),
            exp,
        };
        let token = jsonwebtoken::encode(&Header::new(Algorithm::EdDSA), &claims, &self.enc)
            .map_err(|e| format!("derp ticket mint: {e}"))?;
        Ok((token, exp))
    }
}

/// Build the relay-side verification key from base64(raw 32-byte public key).
pub fn decoding_key_from_public_b64(public_b64: &str) -> Result<DecodingKey, String> {
    let raw = B64
        .decode(public_b64.trim())
        .map_err(|e| format!("derp ticket public key: invalid base64: {e}"))?;
    if raw.len() != 32 {
        return Err(format!(
            "derp ticket public key: expected 32 raw Ed25519 bytes, got {}",
            raw.len()
        ));
    }
    Ok(DecodingKey::from_ed_der(&raw))
}

/// Verify a ticket: EdDSA signature, `aud == "derp"`, unexpired (with leeway).
/// The CALLER must additionally check the first registration frame's pubkey
/// equals `claims.pk` — that binding is what stops a stolen ticket from
/// registering someone else's key.
pub fn verify(token: &str, key: &DecodingKey) -> Result<DerpTicketClaims, String> {
    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.set_audience(&[DERP_AUDIENCE]);
    validation.leeway = DERP_TICKET_LEEWAY_SECS;
    jsonwebtoken::decode::<DerpTicketClaims>(token, key, &validation)
        .map(|d| d.claims)
        .map_err(|e| format!("derp ticket verify: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_signer() -> (DerpTicketSigner, DecodingKey) {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let signer = DerpTicketSigner::from_pkcs8_b64(&B64.encode(pkcs8.as_ref())).unwrap();
        let key = decoding_key_from_public_b64(signer.public_key_b64()).unwrap();
        (signer, key)
    }

    #[test]
    fn mint_verify_roundtrip() {
        let (signer, key) = test_signer();
        let (token, exp) = signer.mint("6a54bf440b4fd609a7356f97", "cHVia2V5").unwrap();
        let claims = verify(&token, &key).expect("fresh ticket verifies");
        assert_eq!(claims.aud, "derp");
        assert_eq!(claims.net, "6a54bf440b4fd609a7356f97");
        assert_eq!(claims.pk, "cHVia2V5");
        assert_eq!(claims.exp, exp);
    }

    #[test]
    fn wrong_key_and_tampering_rejected() {
        let (signer, _) = test_signer();
        let (_, other_key) = test_signer();
        let (token, _) = signer.mint("net", "pk").unwrap();
        assert!(verify(&token, &other_key).is_err(), "foreign key must fail");
        let mut tampered = token.clone();
        tampered.pop();
        assert!(verify(&tampered, &other_key).is_err());
    }

    #[test]
    fn public_key_is_raw_32_bytes() {
        let (signer, _) = test_signer();
        let raw = B64.decode(signer.public_key_b64()).unwrap();
        assert_eq!(raw.len(), 32);
        assert!(decoding_key_from_public_b64("not-base64!!").is_err());
        assert!(decoding_key_from_public_b64(&B64.encode([0u8; 16])).is_err());
    }
}

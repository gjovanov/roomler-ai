//! Overlay L3 data plane — userspace WireGuard mesh (Phase 2+).
//!
//! Feature-gated behind `overlay`. Pulls in `boringtun` for the Noise
//! crypto state machine; everything else (carrier selection, routing,
//! netmap application) is Roomler code.
//!
//! Layout:
//! * [`wg`] — the [`wg::WgDevice`]: a static keypair + a per-peer
//!   `boringtun::noise::Tunn`, each bridged to a [`wg::Carrier`] (a
//!   direct UDP socket or a coturn [`RelayConn`](crate::transport::relay::RelayConn)).
//! * [`router`] — the `overlay_ip → wg_public_key` crypto-routing table
//!   (boringtun's `Tunn` is single-peer, so the `allowed_ips` map lives
//!   here).
//! * [`netmap`] — decode a signed `rc:overlay.netmap` peer into a
//!   routable [`netmap::PeerConfig`].
//! * [`tun`] — the [`tun::TunIo`] OS-NIC seam (`SystemTun` behind
//!   `overlay-l3`; an in-memory mock in tests).
//! * [`bridge`] — the TUN↔`WgDevice` packet pump ([`bridge::run_bridge`]).
//! * [`runtime`] — the node runtime ([`runtime::OverlayRuntime`]): join →
//!   netmap → install WG peers + bring up the TUN + pump packets.
//!
//! Identity: each node owns a stable Curve25519 keypair. The private
//! key never leaves the node; the base64 public key is registered with
//! the coordination server and distributed in the netmap.

pub mod bridge;
pub mod direct;
pub mod netmap;
pub mod relay_link;
pub mod router;
pub mod runtime;
pub mod tun;
pub mod wg;

/// WFP firewall override (Windows + `overlay-l3`): hard-permit the
/// `roomler` adapter so the overlay survives a GPO-locked Defender Firewall.
#[cfg(all(feature = "overlay-l3", windows))]
pub mod wfp;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use boringtun::x25519::{PublicKey, StaticSecret};

/// A node's WireGuard static identity. The secret stays in the node's
/// secure storage; only [`WgKeypair::public_base64`] is published.
#[derive(Clone)]
pub struct WgKeypair {
    pub secret: StaticSecret,
    pub public: PublicKey,
}

impl WgKeypair {
    /// Generate a fresh Curve25519 keypair from the OS CSPRNG.
    pub fn generate() -> Self {
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        let secret = StaticSecret::from(bytes);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    /// Reconstruct from a base64-encoded 32-byte secret (e.g. read back
    /// from config on restart).
    pub fn from_secret_base64(s: &str) -> Option<Self> {
        let raw = B64.decode(s.trim()).ok()?;
        let bytes: [u8; 32] = raw.try_into().ok()?;
        let secret = StaticSecret::from(bytes);
        let public = PublicKey::from(&secret);
        Some(Self { secret, public })
    }

    /// base64 of the 32-byte secret — for persisting to secure storage.
    pub fn secret_base64(&self) -> String {
        B64.encode(self.secret.to_bytes())
    }

    /// base64 of the 32-byte public key — what the netmap distributes.
    pub fn public_base64(&self) -> String {
        encode_public(&self.public)
    }
}

/// base64-encode a WireGuard public key (the netmap wire form).
pub fn encode_public(public: &PublicKey) -> String {
    B64.encode(public.to_bytes())
}

/// Decode a base64 WireGuard public key into raw bytes. `None` on bad
/// base64 or wrong length.
pub fn decode_public(s: &str) -> Option<[u8; 32]> {
    let raw = B64.decode(s.trim()).ok()?;
    raw.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keypair_roundtrips_through_base64() {
        let kp = WgKeypair::generate();
        let restored = WgKeypair::from_secret_base64(&kp.secret_base64()).unwrap();
        assert_eq!(kp.public.to_bytes(), restored.public.to_bytes());
        assert_eq!(kp.public_base64(), restored.public_base64());
    }

    #[test]
    fn public_key_codec_roundtrips() {
        let kp = WgKeypair::generate();
        let b64 = kp.public_base64();
        assert_eq!(decode_public(&b64), Some(kp.public.to_bytes()));
        assert!(decode_public("not-base64!!").is_none());
        assert!(decode_public("c2hvcnQ=").is_none()); // valid b64, wrong len
    }
}

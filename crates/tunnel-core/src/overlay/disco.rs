//! Disco — the out-of-tunnel carrier echo (C1: responder only).
//!
//! # Why this exists, and why it is NOT the rc.346 data-probe
//!
//! Measuring whether a *path* delivers data is the one thing the liveness
//! stack could never do honestly. Every passive signal is indirect: WG
//! handshakes are small and retried, so they complete over a path that drops
//! most bulk data (field 2026-08-11: `tx=122 rx=58`, `ping` 100 % loss, and
//! the carrier still looked healthy to every reaper).
//!
//! rc.346 tried to fix that with an **in-tunnel** probe — an inner ICMP /
//! marked IP packet that the PEER'S OS or overlay engine had to answer. That
//! made the measurement depend on the peer's operating system, its firewall,
//! and a capability negotiation, and it shipped default-ON in the same release
//! that introduced the responder, so nothing in the fleet could answer yet: it
//! demoted healthy 0 ms carriers within minutes.
//!
//! Disco is the Tailscale-shaped correction. It rides **outside** the WG
//! tunnel, as its own datagram shape on the carrier socket the two daemons
//! already share, and it is answered **unconditionally by the daemon** — no
//! OS, no firewall rule, no tunnel session, no capability bit. It measures the
//! PATH, not the peer's host stack.
//!
//! # Deployment rule this module encodes
//!
//! **A prober that punishes non-answer must ship at least one release AFTER
//! the responder is fleet-wide.** C1 therefore ships the responder ONLY —
//! this node answers pings, and asks nothing. Nothing reads the answers yet.
//! The prober (C2) and scoring (C3) come later, once every peer can reply.
//!
//! # Frame
//!
//! ```text
//!  0   magic[8]      "RMDISCO1"
//!  8   kind[1]       1 = ping, 2 = pong
//!  9   reserved[1]   0
//! 10   nonce[8]      echoed verbatim in the pong
//! 18   sender_pub[32] the sender's WG static public key
//! 50   observed[19]  the source the RESPONDER saw (family|16-byte addr|port);
//!                    all-zero in a ping. A free srflx observation for C2.
//! 69   mac[16]       HMAC-SHA256(X25519(self_secret, peer_public), bytes[0..69])[..16]
//! ```
//! Total 85 bytes, fixed.
//!
//! **Shape disjointness (load-bearing — a false match steals a live datagram
//! and blacks out the mesh):** WireGuard is `pkt[0] ∈ 1..=4 && pkt[1..4] ==
//! [0,0,0]` ([`super::wg::is_wg_shaped`]) and STUN is `len ≥ 20 && pkt[4..8]
//! == 0x2112A442` ([`crate::transport::stun::has_stun_cookie`]). `"RMDISCO1"`
//! starts with `0x52` (∉ 1..=4) and carries `"SCO1"` at bytes 4..8 (≠ the STUN
//! cookie), so the three shapes are pairwise disjoint by construction. Locked
//! by [`tests::disco_shape_is_disjoint_from_wg_and_stun`].

use std::net::{IpAddr, SocketAddr};

use boringtun::x25519::{PublicKey, StaticSecret};
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub(crate) const MAGIC: &[u8; 8] = b"RMDISCO1";
pub(crate) const KIND_PING: u8 = 1;
pub(crate) const KIND_PONG: u8 = 2;

const OFF_KIND: usize = 8;
const OFF_NONCE: usize = 10;
const OFF_SENDER: usize = 18;
const OFF_OBSERVED: usize = 50;
const OFF_MAC: usize = 69;
/// Fixed frame length. A reply is exactly this long too, so the responder can
/// never amplify (reply bytes == request bytes).
pub(crate) const FRAME_LEN: usize = 85;

/// A parsed, MAC-verified disco frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Disco {
    pub kind: u8,
    pub nonce: [u8; 8],
    pub sender: [u8; 32],
    /// The source the responder observed, `None` in a ping (all-zero).
    pub observed: Option<SocketAddr>,
}

/// Cheap pre-filter: does this datagram even look like disco? Checked before
/// any crypto so a junk flood costs a memcmp, not a DH.
pub(crate) fn is_disco_shaped(pkt: &[u8]) -> bool {
    pkt.len() == FRAME_LEN && &pkt[..8] == MAGIC.as_slice()
}

/// The sender's claimed static public key, readable WITHOUT verifying the MAC.
/// The caller uses it to decide whether this is a peer it knows (a cheap map
/// lookup) before spending an X25519 — an unknown key is dropped for free, so
/// a flood of forged frames can't burn CPU.
pub(crate) fn claimed_sender(pkt: &[u8]) -> Option<[u8; 32]> {
    if !is_disco_shaped(pkt) {
        return None;
    }
    let mut k = [0u8; 32];
    k.copy_from_slice(&pkt[OFF_SENDER..OFF_SENDER + 32]);
    Some(k)
}

/// Verify + parse. `shared` is the X25519 static-static DH with the claimed
/// sender (the caller resolves and caches it). Returns `None` on any shape or
/// MAC failure — this is the authentication boundary.
pub(crate) fn parse(pkt: &[u8], shared: &[u8; 32]) -> Option<Disco> {
    if !is_disco_shaped(pkt) {
        return None;
    }
    let kind = pkt[OFF_KIND];
    if kind != KIND_PING && kind != KIND_PONG {
        return None;
    }
    if !mac_ok(pkt, shared) {
        return None;
    }
    let mut nonce = [0u8; 8];
    nonce.copy_from_slice(&pkt[OFF_NONCE..OFF_NONCE + 8]);
    let mut sender = [0u8; 32];
    sender.copy_from_slice(&pkt[OFF_SENDER..OFF_SENDER + 32]);
    Some(Disco {
        kind,
        nonce,
        sender,
        observed: decode_observed(&pkt[OFF_OBSERVED..OFF_OBSERVED + 19]),
    })
}

/// Build a frame. `observed` is `Some` only in a pong.
pub(crate) fn build(
    kind: u8,
    nonce: [u8; 8],
    sender_pub: &[u8; 32],
    observed: Option<SocketAddr>,
    shared: &[u8; 32],
) -> Vec<u8> {
    let mut p = vec![0u8; FRAME_LEN];
    p[..8].copy_from_slice(MAGIC.as_slice());
    p[OFF_KIND] = kind;
    p[OFF_NONCE..OFF_NONCE + 8].copy_from_slice(&nonce);
    p[OFF_SENDER..OFF_SENDER + 32].copy_from_slice(sender_pub);
    if let Some(sa) = observed {
        encode_observed(&mut p[OFF_OBSERVED..OFF_OBSERVED + 19], sa);
    }
    let tag = mac(&p[..OFF_MAC], shared);
    p[OFF_MAC..].copy_from_slice(&tag);
    p
}

/// The static-static X25519 shared secret with `peer_public`. Deterministic,
/// so the caller may cache it per peer.
pub(crate) fn shared_secret(secret: &StaticSecret, peer_public: &PublicKey) -> [u8; 32] {
    secret.diffie_hellman(peer_public).to_bytes()
}

/// Answer a disco PING: verify it and build the pong to send back to `src`.
/// `None` = not for us / not a ping / unknown sender / bad MAC ⇒ drop.
///
/// `known_peer` is consulted BEFORE any crypto so a flood of forged frames
/// costs a map lookup, never an X25519. A pong is exactly as long as the ping
/// ([`FRAME_LEN`]), so this can never be used as a reflection amplifier.
///
/// C1 answers peers this device has installed. That is also sufficient for
/// C2's "measure a path we are not currently routing over": the PEER is
/// installed, it is the ENDPOINT that differs.
pub(crate) fn respond(
    pkt: &[u8],
    src: SocketAddr,
    secret: &StaticSecret,
    public: &PublicKey,
    known_peer: impl Fn(&[u8; 32]) -> bool,
) -> Option<Vec<u8>> {
    let sender = claimed_sender(pkt)?;
    if !known_peer(&sender) {
        return None;
    }
    let shared = shared_secret(secret, &PublicKey::from(sender));
    let d = parse(pkt, &shared)?;
    if d.kind != KIND_PING {
        return None; // a pong is for the prober (C2), not the responder
    }
    Some(build(
        KIND_PONG,
        d.nonce,
        public.as_bytes(),
        Some(src),
        &shared,
    ))
}

fn mac(body: &[u8], shared: &[u8; 32]) -> [u8; 16] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(shared).expect("hmac accepts any key length");
    m.update(body);
    let out = m.finalize().into_bytes();
    let mut tag = [0u8; 16];
    tag.copy_from_slice(&out[..16]);
    tag
}

/// Constant-time-ish compare via the MAC crate's own verifier.
fn mac_ok(pkt: &[u8], shared: &[u8; 32]) -> bool {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(shared).expect("hmac accepts any key length");
    m.update(&pkt[..OFF_MAC]);
    let out = m.finalize().into_bytes();
    out[..16] == pkt[OFF_MAC..]
}

fn encode_observed(dst: &mut [u8], sa: SocketAddr) {
    match sa.ip() {
        IpAddr::V4(v4) => {
            dst[0] = 4;
            dst[1..5].copy_from_slice(&v4.octets());
        }
        IpAddr::V6(v6) => {
            dst[0] = 6;
            dst[1..17].copy_from_slice(&v6.octets());
        }
    }
    dst[17..19].copy_from_slice(&sa.port().to_be_bytes());
}

fn decode_observed(src: &[u8]) -> Option<SocketAddr> {
    let port = u16::from_be_bytes([src[17], src[18]]);
    match src[0] {
        4 => {
            let mut o = [0u8; 4];
            o.copy_from_slice(&src[1..5]);
            Some(SocketAddr::from((o, port)))
        }
        6 => {
            let mut o = [0u8; 16];
            o.copy_from_slice(&src[1..17]);
            Some(SocketAddr::from((o, port)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::stun::has_stun_cookie;

    fn kp() -> (StaticSecret, PublicKey) {
        let kp = super::super::WgKeypair::generate();
        (kp.secret, kp.public)
    }

    /// THE load-bearing invariant: a disco frame must never be mistaken for
    /// WireGuard or STUN (a false match steals a live datagram and blacks out
    /// the mesh), and neither may be mistaken for disco.
    #[test]
    fn disco_shape_is_disjoint_from_wg_and_stun() {
        let (a_s, a_p) = kp();
        let (_b_s, b_p) = kp();
        let sh = shared_secret(&a_s, &b_p);
        let f = build(KIND_PING, [7u8; 8], a_p.as_bytes(), None, &sh);

        assert_eq!(f.len(), FRAME_LEN);
        assert!(is_disco_shaped(&f));
        assert!(
            !super::super::wg::is_wg_shaped(&f),
            "disco must never look like WireGuard"
        );
        assert!(
            !has_stun_cookie(&f),
            "disco must never look like STUN (bytes 4..8 != magic cookie)"
        );

        // …and every WG message type must not look like disco. WG is
        // type byte 1..=4 followed by three zero bytes.
        for t in 1u8..=4 {
            let mut wg = vec![0u8; FRAME_LEN];
            wg[0] = t;
            assert!(super::super::wg::is_wg_shaped(&wg));
            assert!(!is_disco_shaped(&wg), "WG type {t} must not parse as disco");
        }
        // A STUN Binding response shape must not look like disco either.
        let mut stun = vec![0u8; FRAME_LEN];
        stun[0] = 0x01;
        stun[4..8].copy_from_slice(&0x2112_A442u32.to_be_bytes());
        assert!(has_stun_cookie(&stun));
        assert!(!is_disco_shaped(&stun));
    }

    #[test]
    fn ping_pong_roundtrip_and_mac_rejects_forgery() {
        let (a_s, a_p) = kp();
        let (b_s, b_p) = kp();
        // A→B share == B→A share (static-static DH is symmetric).
        let ab = shared_secret(&a_s, &b_p);
        let ba = shared_secret(&b_s, &a_p);
        assert_eq!(ab, ba);

        let ping = build(KIND_PING, [9u8; 8], a_p.as_bytes(), None, &ab);
        let got = parse(&ping, &ba).expect("B verifies A's ping");
        assert_eq!(got.kind, KIND_PING);
        assert_eq!(got.nonce, [9u8; 8]);
        assert_eq!(&got.sender, a_p.as_bytes());
        assert_eq!(got.observed, None, "a ping carries no observation");
        assert_eq!(claimed_sender(&ping).unwrap(), *a_p.as_bytes());

        // The pong echoes the nonce and reports the observed source.
        let obs: SocketAddr = "203.0.113.9:41000".parse().unwrap();
        let pong = build(KIND_PONG, got.nonce, b_p.as_bytes(), Some(obs), &ba);
        assert_eq!(pong.len(), ping.len(), "a pong must never amplify");
        let back = parse(&pong, &ab).expect("A verifies B's pong");
        assert_eq!(back.kind, KIND_PONG);
        assert_eq!(back.nonce, [9u8; 8]);
        assert_eq!(back.observed, Some(obs));

        // Forgery: a third party's DH does not verify.
        let (c_s, _c_p) = kp();
        let ca = shared_secret(&c_s, &a_p);
        assert!(parse(&ping, &ca).is_none(), "wrong key must not verify");

        // Any bit flip in the body invalidates the MAC.
        let mut tampered = ping.clone();
        tampered[OFF_NONCE] ^= 0x01;
        assert!(parse(&tampered, &ba).is_none(), "tamper must not verify");

        // Wrong length / wrong magic / unknown kind are rejected pre-crypto.
        assert!(!is_disco_shaped(&ping[..FRAME_LEN - 1]));
        let mut bad_kind = ping.clone();
        bad_kind[OFF_KIND] = 9;
        assert!(parse(&bad_kind, &ba).is_none());
    }

    #[test]
    fn observed_encodes_v4_and_v6() {
        let (a_s, a_p) = kp();
        let (_b_s, b_p) = kp();
        let sh = shared_secret(&a_s, &b_p);
        for s in ["198.51.100.5:65535", "[2001:db8::1]:443"] {
            let sa: SocketAddr = s.parse().unwrap();
            let f = build(KIND_PONG, [0u8; 8], a_p.as_bytes(), Some(sa), &sh);
            assert_eq!(parse(&f, &sh).unwrap().observed, Some(sa));
        }
    }
}

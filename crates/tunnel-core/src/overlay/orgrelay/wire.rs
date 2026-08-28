// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-19 P1 — org-relay wire framing.
//!
//! An org relay forwards WireGuard **ciphertext** between two nodes of one
//! tenant over UDP, keyed by a Geneve (RFC 8926) VNI. This module owns only the
//! framing and the shape rules; nothing here forwards, binds, or holds session
//! state.
//!
//! **Shape disjointness is load-bearing** — the standard is stated in
//! [`super::disco`]: *"a false match steals a live datagram and blacks out the
//! mesh"*. An org-relay frame must be distinguishable from WireGuard, STUN and
//! disco by inspection alone, and the guarantee is proven against those modules'
//! **real** predicates rather than against copies of them (see the tests).
//!
//! Two rules make that work, and neither is decoration:
//!
//! 1. **`Opt Len` MUST be 0**, so byte 0 is always `0x00`. Geneve byte 0 is
//!    `Ver(2) | Opt Len(6)`, so options 1–4 would render as `0x01`–`0x04` —
//!    exactly the WireGuard message types, which
//!    [`is_wg_shaped`](super::wg::is_wg_shaped) reads at that offset. Options
//!    are refused on receive rather than skipped.
//! 2. **The reserved byte MUST be 0.** STUN's magic cookie lives at `pkt[4..8]`,
//!    which for us is `VNI(24) ‖ Reserved(8)`; a frame could only collide by
//!    carrying `VNI == 0x2112A4` *and* reserved `0x42`. Pinning reserved to 0
//!    makes the collision unrepresentable on the wire, and
//!    [`vni_is_mintable`] additionally refuses to ever hand out that VNI so no
//!    session can be created in the ambiguous region either.
//!
//! Frames are **fixed length**. A responder therefore replies with exactly as
//! many bytes as it received and can never amplify — the rule this codebase
//! already applies to disco (`disco::FRAME_LEN`), and one that matters more
//! here because the relay answers unauthenticated packets by design, on a port
//! chosen precisely because corporate egresses permit it.

/// Geneve `Protocol Type` for org-relay frames. An EtherType-shaped field, so
/// this is a private/unassigned value; its job is to keep bytes 2..4 non-zero,
/// which is what stops a frame satisfying WireGuard's four-byte discriminator.
pub const PROTO_ORG_RELAY: u16 = 0x7788;

/// Fixed Geneve header length with `Opt Len == 0`.
pub const HEADER_LEN: usize = 8;

/// Total probe-frame length. Fixed so a reply can never exceed its request.
pub const PROBE_FRAME_LEN: usize = 64;

/// Probe token length (opaque to this module; the server mints it).
pub const PROBE_TOKEN_LEN: usize = 16;

/// The VNI that would place STUN's magic cookie at `pkt[4..8]`. Never minted.
pub const STUN_COOKIE_VNI: u32 = 0x0021_12A4;

/// Largest representable VNI — the wire field is 24 bits, so a `u32` carrying
/// anything above this would silently alias to a different session.
pub const VNI_MAX: u32 = 0x00FF_FFFF;

/// A decoded org-relay Geneve header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrgRelayHeader {
    /// Geneve `O` bit: control (handshake) frame rather than relayed data.
    pub control: bool,
    /// 24-bit Virtual Network Identifier: the relay session.
    pub vni: u32,
}

impl OrgRelayHeader {
    /// Encode into a fixed 8-byte header. `vni` is masked to 24 bits by
    /// construction; callers must have validated it with [`vni_is_mintable`].
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0] = 0x00; // Ver 0, Opt Len 0 -- both halves are invariants
        out[1] = if self.control { 0x80 } else { 0x00 };
        out[2..4].copy_from_slice(&PROTO_ORG_RELAY.to_be_bytes());
        let vni = self.vni & VNI_MAX;
        out[4] = (vni >> 16) as u8;
        out[5] = (vni >> 8) as u8;
        out[6] = vni as u8;
        out[7] = 0x00; // reserved -- pinned, see the module doc
        out
    }

    /// Decode, returning `None` for anything that is not exactly our shape.
    ///
    /// Deliberately total and allocation-free: this runs on an unauthenticated
    /// public UDP port inside a daemon that is SYSTEM/root, so a malformed
    /// packet must be a `None`, never a panic.
    pub fn decode(pkt: &[u8]) -> Option<Self> {
        if pkt.len() < HEADER_LEN {
            return None;
        }
        // Ver must be 0 AND Opt Len must be 0 -- one byte carries both.
        if pkt[0] != 0x00 {
            return None;
        }
        // Only the O (control) bit may be set; C and the reserved bits may not.
        if pkt[1] & 0x7F != 0 {
            return None;
        }
        if pkt[2..4] != PROTO_ORG_RELAY.to_be_bytes() {
            return None;
        }
        // Reserved byte pinned to 0 -- this is what makes STUN disjointness
        // structural rather than probabilistic. Removing it is caught by BOTH
        // `shape_is_disjoint_...` and `stun_cookie_region_...` (mutation-verified).
        if pkt[7] != 0x00 {
            return None;
        }
        let vni = ((pkt[4] as u32) << 16) | ((pkt[5] as u32) << 8) | pkt[6] as u32;
        Some(Self {
            control: pkt[1] & 0x80 != 0,
            vni,
        })
    }
}

/// Is this datagram an org-relay frame? The classifier used by the receive
/// path; a four-byte discriminator plus the pinned reserved byte.
pub fn is_org_relay_shaped(pkt: &[u8]) -> bool {
    OrgRelayHeader::decode(pkt).is_some()
}

/// May this VNI be handed out for a new session?
///
/// Refuses the STUN-cookie value and anything that does not fit 24 bits. A
/// `u32` above [`VNI_MAX`] is not "large", it is a **different session** once
/// truncated onto the wire, which is why this rejects rather than masks.
pub fn vni_is_mintable(vni: u32) -> bool {
    vni <= VNI_MAX && vni != STUN_COOKIE_VNI && vni != 0
}

/// Build a fixed-length reachability probe (P1). Carries the server-minted
/// token and nothing else; the responder echoes the frame verbatim, so request
/// and reply are the same size by construction.
pub fn build_probe(vni: u32, token: &[u8; PROBE_TOKEN_LEN]) -> [u8; PROBE_FRAME_LEN] {
    let mut f = [0u8; PROBE_FRAME_LEN];
    f[..HEADER_LEN].copy_from_slice(&OrgRelayHeader { control: true, vni }.encode());
    f[HEADER_LEN..HEADER_LEN + PROBE_TOKEN_LEN].copy_from_slice(token);
    f
}

/// Parse a probe frame, returning `(vni, token)`. Length is checked exactly:
/// a shorter or longer frame is not a probe, so an echo can never be inflated.
pub fn parse_probe(pkt: &[u8]) -> Option<(u32, [u8; PROBE_TOKEN_LEN])> {
    if pkt.len() != PROBE_FRAME_LEN {
        return None;
    }
    let h = OrgRelayHeader::decode(pkt)?;
    if !h.control {
        return None;
    }
    let mut token = [0u8; PROBE_TOKEN_LEN];
    token.copy_from_slice(&pkt[HEADER_LEN..HEADER_LEN + PROBE_TOKEN_LEN]);
    Some((h.vni, token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::disco;
    use crate::overlay::wg::is_wg_shaped;
    use crate::transport::stun::has_stun_cookie;

    fn probe(vni: u32) -> [u8; PROBE_FRAME_LEN] {
        build_probe(vni, &[0xAB; PROBE_TOKEN_LEN])
    }

    #[test]
    fn header_roundtrips_and_truncates_vni_to_24_bits() {
        for vni in [1u32, 0x00FF_FFFF, 0x0012_3456] {
            let h = OrgRelayHeader { control: true, vni };
            let dec = OrgRelayHeader::decode(&h.encode()).unwrap();
            assert_eq!(dec.vni, vni);
            assert!(dec.control);
        }
        // A value above 24 bits is masked on encode -- which is exactly why
        // `vni_is_mintable` refuses it rather than letting two u32s alias.
        let h = OrgRelayHeader {
            control: false,
            vni: 0x0100_0001,
        };
        assert_eq!(OrgRelayHeader::decode(&h.encode()).unwrap().vni, 1);
        assert!(!vni_is_mintable(0x0100_0001));
    }

    /// The disjointness proof, run against the REAL predicates of the three
    /// other shapes that share these sockets. Testing against local copies
    /// would prove nothing -- if `is_wg_shaped` changes, this must fail.
    #[test]
    fn shape_is_disjoint_from_wg_stun_and_disco_across_every_first_byte() {
        // Sweep byte 0 (Ver|Opt Len) AND byte 7 (reserved) together. Byte 7 is
        // in the sweep deliberately: it is the byte that decides the STUN
        // overlap, so a test that left it at 0 would keep passing if the pin
        // on it were removed -- verified by mutation.
        for b0 in 0u16..=255 {
            for b7 in [0x00u8, 0x42, 0xFF] {
                let mut f = probe(STUN_COOKIE_VNI);
                f[0] = b0 as u8;
                f[7] = b7;
                let ours = is_org_relay_shaped(&f);
                assert_eq!(
                    ours,
                    b0 == 0 && b7 == 0,
                    "byte0={b0:#04x} reserved={b7:#04x}: only Ver 0 / Opt Len 0 with a \
                     zero reserved byte may classify as org-relay"
                );
                if ours {
                    assert!(!is_wg_shaped(&f), "byte0={b0:#04x} collided with WireGuard");
                    assert!(
                        !has_stun_cookie(&f),
                        "byte0={b0:#04x} reserved={b7:#04x} collided with STUN -- even on \
                         the one VNI whose bytes are the magic cookie"
                    );
                    assert!(
                        !disco::is_disco_shaped(&f),
                        "byte0={b0:#04x} collided with disco"
                    );
                }
            }
        }
        // And the converse: a real frame of each other shape is never ours.
        let mut wg = [0u8; 148];
        wg[0] = 1; // handshake initiation
        assert!(is_wg_shaped(&wg) && !is_org_relay_shaped(&wg));

        let mut stun = [0u8; 20];
        stun[1] = 0x01;
        stun[4..8].copy_from_slice(&0x2112_A442u32.to_be_bytes());
        assert!(has_stun_cookie(&stun) && !is_org_relay_shaped(&stun));
    }

    /// Geneve options would put byte 0 in 0x01..=0x04 -- precisely the
    /// WireGuard type range. Options are refused, not skipped.
    #[test]
    fn a_frame_carrying_geneve_options_is_refused() {
        for opt_len in 1u8..=63 {
            let mut f = probe(7);
            f[0] = opt_len; // Ver 0, Opt Len = opt_len
            assert!(
                !is_org_relay_shaped(&f),
                "opt_len={opt_len} must be refused"
            );
            if (1..=4).contains(&opt_len) {
                assert!(
                    is_wg_shaped(&f[..4]) || f[1] != 0,
                    "opt_len={opt_len} lands in the WireGuard type range"
                );
            }
        }
    }

    /// The STUN cookie can only appear at 4..8 if the reserved byte is 0x42.
    /// Pinning it to 0 makes the collision unrepresentable; refusing to mint
    /// the VNI closes the other half.
    #[test]
    fn stun_cookie_region_is_unrepresentable_and_the_vni_is_never_minted() {
        let mut f = probe(STUN_COOKIE_VNI);
        assert_eq!(&f[4..7], &[0x21, 0x12, 0xA4]);
        assert_eq!(f[7], 0x00, "reserved must be pinned to 0");
        assert!(
            !has_stun_cookie(&f),
            "reserved=0 keeps it out of STUN's region"
        );

        // Forge the byte an attacker would need: it stops being our shape.
        f[7] = 0x42;
        assert!(has_stun_cookie(&f));
        assert!(
            !is_org_relay_shaped(&f),
            "reserved!=0 must not classify as ours"
        );

        assert!(!vni_is_mintable(STUN_COOKIE_VNI));
    }

    #[test]
    fn reserved_and_critical_bits_are_refused() {
        for bit in [0x01u8, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40] {
            let mut f = probe(9);
            f[1] |= bit;
            assert!(!is_org_relay_shaped(&f), "bit {bit:#04x} must be refused");
        }
        // Only the O (control) bit is legal.
        let mut f = probe(9);
        f[1] = 0x80;
        assert!(is_org_relay_shaped(&f));
    }

    #[test]
    fn wrong_protocol_type_is_refused() {
        let mut f = probe(9);
        f[2] = 0x00;
        f[3] = 0x00;
        assert!(!is_org_relay_shaped(&f));
    }

    /// A responder echoes the frame verbatim, so reply bytes == request bytes.
    /// Anything but an exact-length frame is not a probe, which is what stops
    /// a short request eliciting a long reply.
    #[test]
    fn probe_is_fixed_length_so_a_reply_can_never_amplify() {
        let f = probe(11);
        assert_eq!(f.len(), PROBE_FRAME_LEN);
        let (vni, token) = parse_probe(&f).unwrap();
        assert_eq!(vni, 11);
        assert_eq!(token, [0xAB; PROBE_TOKEN_LEN]);

        assert!(parse_probe(&f[..PROBE_FRAME_LEN - 1]).is_none());
        let mut long = f.to_vec();
        long.push(0);
        assert!(parse_probe(&long).is_none());
    }

    #[test]
    fn a_data_frame_is_not_accepted_as_a_probe() {
        let mut f = probe(12);
        f[1] = 0x00; // clear the control bit
        assert!(is_org_relay_shaped(&f));
        assert!(parse_probe(&f).is_none());
    }

    /// Total over arbitrary input: this parser runs on an unauthenticated
    /// public UDP port inside a SYSTEM/root daemon, so a panic here is a
    /// remote daemon kill, not a parse bug.
    #[test]
    fn decoding_arbitrary_bytes_never_panics() {
        let mut seed = 0x1234_5678u32;
        for len in 0usize..80 {
            for _ in 0..64 {
                let mut buf = vec![0u8; len];
                for b in buf.iter_mut() {
                    // xorshift; deterministic, no rand dependency
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    *b = seed as u8;
                }
                let _ = is_org_relay_shaped(&buf);
                let _ = parse_probe(&buf);
                let _ = OrgRelayHeader::decode(&buf);
            }
        }
    }

    #[test]
    fn zero_vni_is_not_mintable() {
        assert!(!vni_is_mintable(0));
        assert!(vni_is_mintable(1));
        assert!(vni_is_mintable(VNI_MAX));
        assert!(!vni_is_mintable(VNI_MAX + 1));
    }
}

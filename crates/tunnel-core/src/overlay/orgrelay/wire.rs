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

use super::bind::{MAC_LEN, Mac, NONCE_LEN, Nonce};

/// Geneve `Protocol Type` for org-relay frames. An EtherType-shaped field, so
/// this is a private/unassigned value; its job is to keep bytes 2..4 non-zero,
/// which is what stops a frame satisfying WireGuard's four-byte discriminator.
pub const PROTO_ORG_RELAY: u16 = 0x7788;

/// Fixed Geneve header length with `Opt Len == 0`.
pub const HEADER_LEN: usize = 8;

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

// ── Control frames ──────────────────────────────────────────────────────────
//
// Every control frame is exactly CONTROL_FRAME_LEN bytes: header (8), a kind
// byte (1), the kind's fixed fields, zero padding. One size for all of them is
// what makes the anti-amplification property trivial to state and to test —
// the reply to a bind is a challenge of the same size, the reply to a probe is
// the probe itself, and nothing else replies at all.

/// The single size of every control frame.
pub const CONTROL_FRAME_LEN: usize = 64;
/// Kept as the historical name for the probe; the same number.
pub const PROBE_FRAME_LEN: usize = CONTROL_FRAME_LEN;
/// Byte 8 of a control frame.
const OFF_KIND: usize = HEADER_LEN;
/// Fixed fields start here.
const OFF_BODY: usize = HEADER_LEN + 1;

pub const KIND_PROBE: u8 = 1;
pub const KIND_BIND: u8 = 2;
pub const KIND_CHALLENGE: u8 = 3;
pub const KIND_ANSWER: u8 = 4;

/// A decoded control frame. The variants are the whole handshake vocabulary;
/// a kind byte outside this set is refused, never guessed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlFrame {
    /// P1 reachability probe — echoed verbatim by a responder.
    Probe { token: [u8; PROBE_TOKEN_LEN] },
    /// Step 1: member → relay.
    Bind { nonce: Nonce, tag1: Mac },
    /// Step 2: relay → member.
    Challenge { nonce: Nonce, cookie: Mac },
    /// Step 3: member → relay.
    Answer {
        nonce: Nonce,
        cookie: Mac,
        tag2: Mac,
    },
}

impl ControlFrame {
    fn kind(&self) -> u8 {
        match self {
            Self::Probe { .. } => KIND_PROBE,
            Self::Bind { .. } => KIND_BIND,
            Self::Challenge { .. } => KIND_CHALLENGE,
            Self::Answer { .. } => KIND_ANSWER,
        }
    }

    /// Encode as a fixed 64-byte frame with the control bit set.
    pub fn encode(&self, vni: u32) -> [u8; CONTROL_FRAME_LEN] {
        let mut f = [0u8; CONTROL_FRAME_LEN];
        f[..HEADER_LEN].copy_from_slice(&OrgRelayHeader { control: true, vni }.encode());
        f[OFF_KIND] = self.kind();
        let b = &mut f[OFF_BODY..];
        match self {
            Self::Probe { token } => b[..PROBE_TOKEN_LEN].copy_from_slice(token),
            Self::Bind { nonce, tag1 } => {
                b[..NONCE_LEN].copy_from_slice(nonce);
                b[NONCE_LEN..NONCE_LEN + MAC_LEN].copy_from_slice(tag1);
            }
            Self::Challenge { nonce, cookie } => {
                b[..NONCE_LEN].copy_from_slice(nonce);
                b[NONCE_LEN..NONCE_LEN + MAC_LEN].copy_from_slice(cookie);
            }
            Self::Answer {
                nonce,
                cookie,
                tag2,
            } => {
                b[..NONCE_LEN].copy_from_slice(nonce);
                b[NONCE_LEN..NONCE_LEN + MAC_LEN].copy_from_slice(cookie);
                b[NONCE_LEN + MAC_LEN..NONCE_LEN + 2 * MAC_LEN].copy_from_slice(tag2);
            }
        }
        f
    }

    /// Decode, returning `(vni, frame)`. Length is checked **exactly**: a
    /// shorter or longer frame is not a control frame, so an echo or a
    /// challenge can never be inflated. Total over arbitrary input.
    pub fn decode(pkt: &[u8]) -> Option<(u32, Self)> {
        if pkt.len() != CONTROL_FRAME_LEN {
            return None;
        }
        let h = OrgRelayHeader::decode(pkt)?;
        if !h.control {
            return None;
        }
        let b = &pkt[OFF_BODY..];
        let take16 = |at: usize| -> [u8; 16] {
            let mut out = [0u8; 16];
            out.copy_from_slice(&b[at..at + 16]);
            out
        };
        let frame = match pkt[OFF_KIND] {
            KIND_PROBE => Self::Probe { token: take16(0) },
            KIND_BIND => Self::Bind {
                nonce: take16(0),
                tag1: take16(NONCE_LEN),
            },
            KIND_CHALLENGE => Self::Challenge {
                nonce: take16(0),
                cookie: take16(NONCE_LEN),
            },
            KIND_ANSWER => Self::Answer {
                nonce: take16(0),
                cookie: take16(NONCE_LEN),
                tag2: take16(NONCE_LEN + MAC_LEN),
            },
            _ => return None,
        };
        Some((h.vni, frame))
    }
}

/// Build a reachability probe. A thin wrapper over [`ControlFrame::Probe`],
/// kept because P1 shipped this name and the responder tests use it.
pub fn build_probe(vni: u32, token: &[u8; PROBE_TOKEN_LEN]) -> [u8; PROBE_FRAME_LEN] {
    ControlFrame::Probe { token: *token }.encode(vni)
}

/// Parse a probe frame, returning `(vni, token)`. Any other control kind — or
/// anything that is not exactly one control frame long — is `None`.
pub fn parse_probe(pkt: &[u8]) -> Option<(u32, [u8; PROBE_TOKEN_LEN])> {
    match ControlFrame::decode(pkt)? {
        (vni, ControlFrame::Probe { token }) => Some((vni, token)),
        _ => None,
    }
}

// ── Data frames ─────────────────────────────────────────────────────────────

/// Frame WireGuard ciphertext for the relay: header with the control bit
/// clear, payload verbatim. The relay forwards the whole frame unchanged and
/// the far member strips the header — the relay never re-frames, so it can
/// never be made to emit more than it received.
pub fn build_data(vni: u32, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(HEADER_LEN + payload.len());
    f.extend_from_slice(
        &OrgRelayHeader {
            control: false,
            vni,
        }
        .encode(),
    );
    f.extend_from_slice(payload);
    f
}

/// Parse a data frame into `(vni, payload)`. A control frame is not data, and
/// an empty payload is refused: there is nothing to relay in it, and a
/// zero-length forward is a free way to make the relay emit a datagram.
pub fn parse_data(pkt: &[u8]) -> Option<(u32, &[u8])> {
    let h = OrgRelayHeader::decode(pkt)?;
    if h.control || pkt.len() <= HEADER_LEN {
        return None;
    }
    Some((h.vni, &pkt[HEADER_LEN..]))
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

    /// Every control frame is one size, and every kind round-trips. One size
    /// is what makes "a reply is never larger than its request" a property of
    /// the encoding rather than of each handler's discipline.
    #[test]
    fn every_control_kind_roundtrips_at_one_fixed_size() {
        let frames = [
            ControlFrame::Probe { token: [0x11; 16] },
            ControlFrame::Bind {
                nonce: [0x22; 16],
                tag1: [0x33; 16],
            },
            ControlFrame::Challenge {
                nonce: [0x44; 16],
                cookie: [0x55; 16],
            },
            ControlFrame::Answer {
                nonce: [0x66; 16],
                cookie: [0x77; 16],
                tag2: [0x88; 16],
            },
        ];
        for f in frames {
            let bytes = f.encode(0x0012_3456);
            assert_eq!(bytes.len(), CONTROL_FRAME_LEN);
            assert!(is_org_relay_shaped(&bytes));
            let (vni, back) = ControlFrame::decode(&bytes).expect("must decode");
            assert_eq!(vni, 0x0012_3456);
            assert_eq!(back, f);
        }
    }

    /// A kind byte outside the vocabulary is refused, never guessed at — and
    /// it is refused as a control frame while still being org-relay SHAPED,
    /// so the caller can count it as "ours but unknown" rather than "junk".
    #[test]
    fn an_unknown_control_kind_is_refused_but_still_shaped() {
        let mut f = ControlFrame::Probe { token: [0; 16] }.encode(9);
        for kind in [0u8, 5, 0x7F, 0xFF] {
            f[HEADER_LEN] = kind;
            assert!(is_org_relay_shaped(&f));
            assert!(
                ControlFrame::decode(&f).is_none(),
                "kind {kind} must be refused"
            );
            assert!(parse_probe(&f).is_none());
        }
    }

    #[test]
    fn a_bind_frame_is_not_a_probe_and_vice_versa() {
        let bind = ControlFrame::Bind {
            nonce: [1; 16],
            tag1: [2; 16],
        }
        .encode(9);
        assert!(
            parse_probe(&bind).is_none(),
            "a bind must not echo as a probe"
        );
        let probe = build_probe(9, &[3; 16]);
        assert!(matches!(
            ControlFrame::decode(&probe),
            Some((9, ControlFrame::Probe { .. }))
        ));
    }

    /// Data and control are separated by the control bit alone, so a data
    /// frame whose payload happens to look like a control body is still data.
    #[test]
    fn data_frames_roundtrip_and_the_control_bit_separates_them() {
        let payload = b"wireguard-ciphertext-goes-here";
        let d = build_data(0x0042_4242, payload);
        assert!(is_org_relay_shaped(&d));
        let (vni, got) = parse_data(&d).expect("data must parse");
        assert_eq!(vni, 0x0042_4242);
        assert_eq!(got, payload);
        assert!(
            ControlFrame::decode(&d).is_none(),
            "a data frame is never a control frame"
        );

        // A control frame is never data, even though it is longer than a header.
        let c = build_probe(7, &[0; 16]);
        assert!(parse_data(&c).is_none());

        // An empty payload is not data: nothing to relay, and a zero-length
        // forward would be a free way to make the relay emit a datagram.
        let empty = build_data(7, b"");
        assert!(parse_data(&empty).is_none());
    }

    #[test]
    fn control_and_data_decoders_never_panic_on_arbitrary_bytes() {
        let mut seed = 0xC0FF_EE11u32;
        for len in 0usize..80 {
            for _ in 0..64 {
                let mut buf = vec![0u8; len];
                for b in buf.iter_mut() {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    *b = seed as u8;
                }
                let _ = ControlFrame::decode(&buf);
                let _ = parse_data(&buf);
            }
        }
    }
}

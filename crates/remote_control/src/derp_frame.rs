// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! What a DERP payload is — read the same way by the agent's DERP mux
//! (`tunnel_core::transport::derp`, which re-exports this), the API pods'
//! `/derp` and the regional PoPs' `derp-relay`.
//!
//! One definition on purpose: a classifier that drifts between a relay and
//! the mux routes a frame to the wrong consumer (#2002), or holds back a
//! WireGuard handshake the relay allowance must always let through (FR-115).
//! It lives here because this is the one crate every one of them links
//! without dragging in the agent's transport stack.

/// The disco magic. `overlay::disco::MAGIC` is the same bytes, and a
/// feature-gated test in `tunnel_core::transport::derp` asserts the two
/// never drift (`transport` must compile without the `overlay` feature).
pub const DISCO_MAGIC: &[u8; 8] = b"RMDISCO1";

/// Which consumer an inbound DERP payload belongs to. WireGuard frames start
/// with their LE u32 message type — `[1..=4, 0, 0, 0]` — and disco frames
/// with the 8-byte `RMDISCO1` magic (designed disjoint from WG, doc'd in
/// `overlay::disco`); both belong to the carrier/WG consumer. EVERYTHING else
/// (QUIC long headers ≥0xC0, short headers 0x40..=0x7F, version-negotiation
/// 0x80..=0xBF) is tunnel traffic — but only when a tunnel consumer is
/// registered; otherwise it falls through to the WG consumer exactly as
/// before R4 (boringtun discards non-WG bytes — harmless, and preserves the
/// legacy path for anything unexpected). A QUIC short-header packet CAN start
/// with 0x52 ('R'), which is why the disco check is the full 8-byte magic and
/// runs first.
///
/// ⚠️ #2002 — WireGuard is the WHOLE 4-byte header, never the first byte
/// alone. quinn greases QUIC's fixed bit by default (RFC 9287): an end whose
/// peer advertised it clears the bit on about half of what it sends, and a
/// short header then starts anywhere below 0x40, 1..=4 among it — measured at
/// 2.6-4.0% of a transfer's datagrams (`quic::tests::
/// quic_over_derp_never_clears_the_fixed_bit`). Read by the first byte alone,
/// those went to the WG consumer and were lost, and the server's tunnel
/// shaper let them by. A greased QUIC packet that still passes must also open
/// its random connection ID with three zero bytes.
pub fn payload_is_wg_or_disco(payload: &[u8]) -> bool {
    is_wg_header(payload) || payload.len() >= 8 && &payload[..8] == DISCO_MAGIC
}

/// WireGuard's own message header: the type (1 handshake initiation, 2
/// response, 3 cookie reply, 4 transport data) and three reserved bytes, zero
/// in every WireGuard message.
pub fn is_wg_header(payload: &[u8]) -> bool {
    matches!(payload, [1..=4, 0, 0, 0, ..])
}

/// FR-115 P4a — a BULK frame, the only kind the relay allowance may hold
/// back: WireGuard transport data (type 4), or anything that is neither
/// WireGuard nor disco (the tunnel-class payloads). A handshake (types 1–3)
/// and disco are control, and pass whatever the allowance says: a throttle
/// that delayed a handshake past its retry window would turn "slow" into
/// "down" for exactly the hosts with no other path.
pub fn frame_is_bulk(payload: &[u8]) -> bool {
    !payload_is_wg_or_disco(payload) || matches!(payload, [4, 0, 0, 0, ..])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only bulk meets the throttle: a WireGuard handshake, its response, a
    /// cookie reply and disco always pass; transport data and tunnel-class
    /// payloads do not.
    #[test]
    fn only_bulk_frames_meet_the_throttle() {
        for t in 1u8..=3 {
            assert!(
                !frame_is_bulk(&[t, 0, 0, 0, 9, 9, 9, 9]),
                "WireGuard type {t} is control"
            );
        }
        assert!(
            frame_is_bulk(&[4, 0, 0, 0, 9, 9, 9, 9]),
            "WireGuard transport data is bulk"
        );
        let mut disco = DISCO_MAGIC.to_vec();
        disco.extend_from_slice(&[1, 2, 3]);
        assert!(!frame_is_bulk(&disco), "disco is control");
        assert!(
            frame_is_bulk(&[0x41, 7, 7, 7, 7]),
            "a QUIC short header (tunnel class) is bulk"
        );
        assert!(
            frame_is_bulk(&[1, 0, 0, 1]),
            "one byte off WireGuard's header is not WireGuard"
        );
    }

    /// #2002 — the whole header, never the first byte alone.
    #[test]
    fn wireguard_is_the_whole_header() {
        assert!(payload_is_wg_or_disco(&[2, 0, 0, 0, 0xAA]));
        assert!(
            !payload_is_wg_or_disco(&[2, 0x9c, 0x41, 0x7e]),
            "greased QUIC"
        );
        assert!(!payload_is_wg_or_disco(&[1, 0, 0]), "a header cut short");
        assert!(!payload_is_wg_or_disco(&[5, 0, 0, 0, 1]), "type 5 is none");
    }
}

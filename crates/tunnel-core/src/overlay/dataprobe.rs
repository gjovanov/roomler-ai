//! Overlay carrier data-probe — a bidirectional data-plane liveness check.
//!
//! The passive liveness signals cannot tell a HEALTHY direct carrier from a
//! HEAVILY-LOSSY one: WG handshakes are small + retried so they complete over
//! a path that drops most data (field 2026-08-11, pc50045/Check Point:
//! `tx=122 rx=58`, recent `ping` 100 % loss, yet the carrier stayed "direct").
//! Both existing reapers are defeated — the Stage-2 poke's proof resets on the
//! completed rekey handshake, and the one-way `bad_sweeps` counter oscillates
//! because the ~120 s rekey PAUSES tx (resets the strike) and a lossy-path
//! trickle occasionally delivers a packet (also resets it).
//!
//! The only signal that can't be fooled is an ACTIVELY-PROBED round trip whose
//! request AND reply both ride the real WG DATA path. We build a standard
//! **ICMP echo request** to the peer's overlay IP and send it encapsulated
//! over the peer's session: any overlay peer's OS answers a ping to its own
//! overlay IP (so NO capability negotiation is needed — old peers reply too),
//! and the echo REPLY comes back over the same carrier. Sustained probe loss
//! on a direct carrier ⇒ the data path is dead even though handshakes pass ⇒
//! demote to relay (which is bidirectionally reliable for such a pair).
//!
//! The identifier field carries a fixed [`PROBE_IDENT`] so the recv path
//! recognises OUR probe's replies (and suppresses them from the TUN) without
//! mistaking a user's own `ping` (random identifier) for a probe.

use std::net::Ipv4Addr;

/// ICMP identifier marking a packet as an overlay carrier data-probe
/// (`0x524D` = "RM"). A user `ping` uses a per-process/random identifier, so a
/// collision that both matches this AND carries a live probe's sequence is
/// astronomically unlikely and at worst credits one probe.
pub(crate) const PROBE_IDENT: u16 = 0x524D;

/// Build an ICMP echo REQUEST IPv4 packet (`src` → `dst`) carrying
/// [`PROBE_IDENT`] and `seq`, ready to hand to `WgSender::send_ip_packet`
/// (which routes it to `dst`'s session and encapsulates it). 28 bytes:
/// 20-byte IPv4 header + 8-byte ICMP echo header, no payload.
pub(crate) fn echo_request(src: Ipv4Addr, dst: Ipv4Addr, seq: u16) -> Vec<u8> {
    let mut p = vec![0u8; 28];
    // IPv4 header
    p[0] = 0x45; // version 4, IHL 5
    p[3] = 28; // total length low byte (high byte 0)
    p[8] = 64; // TTL
    p[9] = 1; // protocol = ICMP
    p[12..16].copy_from_slice(&src.octets());
    p[16..20].copy_from_slice(&dst.octets());
    let ip_csum = checksum(&p[..20]);
    p[10..12].copy_from_slice(&ip_csum.to_be_bytes());
    // ICMP echo request
    p[20] = 8; // type = echo request
    // p[21] code = 0
    p[24..26].copy_from_slice(&PROBE_IDENT.to_be_bytes());
    p[26..28].copy_from_slice(&seq.to_be_bytes());
    let icmp_csum = checksum(&p[20..28]);
    p[22..24].copy_from_slice(&icmp_csum.to_be_bytes());
    p
}

/// If `pkt` is an ICMP echo REPLY carrying [`PROBE_IDENT`] (i.e. the reply to
/// one of OUR probes), return its sequence. `None` for anything else — a real
/// data packet, a user's ping (different identifier), or an echo *request*
/// (which must pass through to the TUN so our own OS answers the peer's
/// probe). Header-length tolerant (honours IHL) and bounds-checked.
pub(crate) fn parse_echo_reply(pkt: &[u8]) -> Option<u16> {
    if pkt.len() < 20 || (pkt[0] >> 4) != 4 || pkt[9] != 1 {
        return None; // not IPv4/ICMP
    }
    let ihl = ((pkt[0] & 0x0f) as usize) * 4;
    let icmp = pkt.get(ihl..)?;
    if icmp.len() < 8 || icmp[0] != 0 {
        return None; // not an ICMP echo REPLY (type 0)
    }
    let ident = u16::from_be_bytes([icmp[4], icmp[5]]);
    if ident != PROBE_IDENT {
        return None; // not ours
    }
    Some(u16::from_be_bytes([icmp[6], icmp[7]]))
}

/// Overlay-native echo — the non-ICMP half of the HYBRID probe, for peers
/// advertising `supports_overlay_echo`. A netstack-only peer has no OS ICMP
/// responder, so the ICMP probe would false-positive-demote it; this variant
/// is answered by the peer's overlay ENGINE itself (inline in its decrypt
/// path), guaranteeing a reply whenever data genuinely flows. Format: a full
/// IPv4 header (required for egress routing) with protocol 253 (RFC 3692
/// experimental) and a 5-byte body `[MAGIC u16 | kind u8 | seq u16]`.
pub(crate) const OVERLAY_ECHO_PROTO: u8 = 253;
pub(crate) const OVERLAY_ECHO_MAGIC: u16 = 0x524D; // "RM"
pub(crate) const ECHO_KIND_REQUEST: u8 = 1;
pub(crate) const ECHO_KIND_REPLY: u8 = 2;

/// Build an overlay-native echo packet (`src` → `dst`, `kind`, `seq`).
/// 25 bytes: 20-byte IPv4 header + 5-byte marked body.
pub(crate) fn build_overlay_echo(src: Ipv4Addr, dst: Ipv4Addr, kind: u8, seq: u16) -> Vec<u8> {
    let mut p = vec![0u8; 25];
    p[0] = 0x45;
    p[3] = 25; // total length (high byte 0)
    p[8] = 64; // TTL
    p[9] = OVERLAY_ECHO_PROTO;
    p[12..16].copy_from_slice(&src.octets());
    p[16..20].copy_from_slice(&dst.octets());
    let csum = checksum(&p[..20]);
    p[10..12].copy_from_slice(&csum.to_be_bytes());
    p[20..22].copy_from_slice(&OVERLAY_ECHO_MAGIC.to_be_bytes());
    p[22] = kind;
    p[23..25].copy_from_slice(&seq.to_be_bytes());
    p
}

/// If `pkt` is an overlay-native echo (proto 253 + magic), return
/// `(kind, seq)`. `None` for anything else — including other uses of the
/// experimental protocol number (magic-gated). Honours IHL; bounds-checked.
pub(crate) fn parse_overlay_echo(pkt: &[u8]) -> Option<(u8, u16)> {
    if pkt.len() < 20 || (pkt[0] >> 4) != 4 || pkt[9] != OVERLAY_ECHO_PROTO {
        return None;
    }
    let ihl = ((pkt[0] & 0x0f) as usize) * 4;
    let body = pkt.get(ihl..)?;
    if body.len() < 5 || u16::from_be_bytes([body[0], body[1]]) != OVERLAY_ECHO_MAGIC {
        return None;
    }
    Some((body[2], u16::from_be_bytes([body[3], body[4]])))
}

/// Turn a received overlay-echo REQUEST into its REPLY in place: swap
/// src/dst, flip the kind byte, recompute the IPv4 checksum. The caller
/// re-encapsulates it over the same carrier the request arrived on.
pub(crate) fn overlay_echo_reply_in_place(pkt: &mut [u8]) {
    debug_assert!(pkt.len() >= 25);
    let ihl = ((pkt[0] & 0x0f) as usize) * 4;
    // Swap src (12..16) and dst (16..20).
    for i in 0..4 {
        pkt.swap(12 + i, 16 + i);
    }
    pkt[ihl + 2] = ECHO_KIND_REPLY;
    // Recompute the header checksum (zero it first).
    pkt[10] = 0;
    pkt[11] = 0;
    let csum = checksum(&pkt[..ihl.min(pkt.len())]);
    pkt[10..12].copy_from_slice(&csum.to_be_bytes());
}

/// Standard RFC 1071 one's-complement checksum.
fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_request_is_well_formed_ipv4_icmp() {
        let src = Ipv4Addr::new(100, 65, 4, 1);
        let dst = Ipv4Addr::new(100, 65, 4, 28);
        let p = echo_request(src, dst, 7);
        assert_eq!(p.len(), 28);
        assert_eq!(p[0], 0x45);
        assert_eq!(p[9], 1); // ICMP
        assert_eq!(&p[12..16], &src.octets());
        assert_eq!(&p[16..20], &dst.octets());
        assert_eq!(p[20], 8); // echo request
        // IPv4 + ICMP checksums verify (sum incl. the checksum field == 0).
        assert_eq!(checksum(&p[..20]), 0);
        assert_eq!(checksum(&p[20..28]), 0);
    }

    #[test]
    fn parse_reply_matches_only_our_probe() {
        // Turn a request into a reply (type 8 → 0, re-checksum) and parse it.
        let mut reply = echo_request(
            Ipv4Addr::new(100, 65, 4, 28),
            Ipv4Addr::new(100, 65, 4, 1),
            42,
        );
        reply[20] = 0; // echo reply
        reply[22..24].copy_from_slice(&[0, 0]);
        let c = checksum(&reply[20..28]);
        reply[22..24].copy_from_slice(&c.to_be_bytes());
        assert_eq!(parse_echo_reply(&reply), Some(42));

        // An echo REQUEST (type 8) is not a reply — must pass through.
        let req = echo_request(Ipv4Addr::new(1, 2, 3, 4), Ipv4Addr::new(5, 6, 7, 8), 1);
        assert_eq!(parse_echo_reply(&req), None);

        // A different identifier (a user's ping) is not ours.
        let mut other = req.clone();
        other[20] = 0; // reply
        other[24..26].copy_from_slice(&0x1234u16.to_be_bytes());
        assert_eq!(parse_echo_reply(&other), None);

        // Non-ICMP / too-short → None.
        assert_eq!(parse_echo_reply(&[0x45, 0, 0]), None);
        assert_eq!(parse_echo_reply(&[]), None);
    }

    #[test]
    fn overlay_echo_roundtrip_and_gating() {
        let a = Ipv4Addr::new(100, 65, 4, 1);
        let b = Ipv4Addr::new(100, 65, 0, 5);
        let req = build_overlay_echo(a, b, ECHO_KIND_REQUEST, 9);
        assert_eq!(req.len(), 25);
        assert_eq!(req[9], OVERLAY_ECHO_PROTO);
        assert_eq!(checksum(&req[..20]), 0); // header checksum verifies
        assert_eq!(parse_overlay_echo(&req), Some((ECHO_KIND_REQUEST, 9)));

        // The receiver turns the request into a reply in place: src/dst
        // swapped, kind flipped, checksum still valid, seq preserved.
        let mut reply = req.clone();
        overlay_echo_reply_in_place(&mut reply);
        assert_eq!(&reply[12..16], &b.octets());
        assert_eq!(&reply[16..20], &a.octets());
        assert_eq!(checksum(&reply[..20]), 0);
        assert_eq!(parse_overlay_echo(&reply), Some((ECHO_KIND_REPLY, 9)));

        // Gating: same proto but wrong magic → not ours; ICMP → not an
        // overlay echo; short/garbage → None.
        let mut foreign = req.clone();
        foreign[20..22].copy_from_slice(&0x1111u16.to_be_bytes());
        assert_eq!(parse_overlay_echo(&foreign), None);
        assert_eq!(parse_overlay_echo(&echo_request(a, b, 1)), None);
        assert_eq!(parse_overlay_echo(&[0x45, 0, 0]), None);

        // An ICMP parser never claims an overlay echo (disjoint protocols).
        assert_eq!(parse_echo_reply(&req), None);
        assert_eq!(parse_echo_reply(&reply), None);
    }
}

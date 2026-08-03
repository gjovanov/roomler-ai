//! P4 — per-source ingress rules: the finest tier of the overlay ACL.
//!
//! [`LocalScope`](super::router::LocalScope) already limits every peer to the
//! subnets this node advertises, and [`Router::rpf_check`](super::router::Router::rpf_check)
//! stops source forgery. Neither can express *"**Alice** may reach 10.66.0.0/24,
//! but only on :22"* — that needs a per-(recipient, peer) rule set, which the
//! server compiles from `overlay_policies` and ships on the peer's netmap entry.
//!
//! This module is the pure half: parse what a packet is addressed to, and decide
//! whether a rule set permits it. No I/O, no state — so the whole decision table
//! is unit-testable against synthetic packets.
//!
//! # Why the parser has to exist
//!
//! `Router::dst_of_ip_packet` reads the destination and nothing else. Port and
//! protocol were carried on `OverlayRule` from the start but were **stored and
//! distributed, never enforced**, precisely because no layer could see an L4
//! header. This is that layer.

use std::net::IpAddr;

use roomler_ai_remote_control::models::{OverlayRule, ProtocolKind};

/// IP protocol numbers we care about. Anything else is `Other` — matched only
/// by a rule that doesn't narrow protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum L4Proto {
    Tcp,
    Udp,
    Other(u8),
}

/// What the ACL needs from a packet beyond its destination address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct L4 {
    pub proto: L4Proto,
    /// `None` when the packet carries no reachable L4 header: ICMP, a non-first
    /// IPv4 fragment, an unparseable v6 extension-header chain, or a truncated
    /// packet. A port-narrowed rule must NOT match such a packet — see
    /// [`rules_permit`].
    pub dst_port: Option<u16>,
}

/// Max IPv6 extension headers to walk before giving up. A real packet has ≤2;
/// the bound exists so a hostile header chain can't spin the recv loop.
const MAX_V6_EXT_HEADERS: usize = 8;

/// IPv6 extension-header next-header values that carry a `(next, len)` pair.
fn v6_ext_len(nh: u8, buf: &[u8], off: usize) -> Option<usize> {
    match nh {
        // Hop-by-hop, routing, destination options, mobility: len is in
        // 8-octet units NOT counting the first 8.
        0 | 43 | 60 | 135 => buf.get(off + 1).map(|l| (*l as usize + 1) * 8),
        // Fragment header is a fixed 8 bytes.
        44 => Some(8),
        // Authentication header: len is in 4-octet units, not counting 2.
        51 => buf.get(off + 1).map(|l| (*l as usize + 2) * 4),
        _ => None,
    }
}

/// Parse the protocol + destination port of a raw IP packet.
///
/// Length-guarded throughout: every index is bounds-checked, so a truncated or
/// hostile packet yields `None`/`dst_port: None` rather than panicking on the
/// recv path. Returns `None` only when the packet isn't a recognisable IP
/// packet at all.
pub fn parse_l4(packet: &[u8]) -> Option<L4> {
    match packet.first()? >> 4 {
        4 => {
            if packet.len() < 20 {
                return None;
            }
            // IHL is in 32-bit words and must cover the base header.
            let ihl = ((packet[0] & 0x0f) as usize) * 4;
            if ihl < 20 {
                return None;
            }
            let proto = proto_of(packet[9]);
            // A non-first fragment (offset != 0) has no L4 header to read.
            let frag_off = (((packet[6] & 0x1f) as u16) << 8) | packet[7] as u16;
            let dst_port = if frag_off == 0 {
                port_at(packet, ihl, proto)
            } else {
                None
            };
            Some(L4 { proto, dst_port })
        }
        6 => {
            if packet.len() < 40 {
                return None;
            }
            let mut nh = packet[6];
            let mut off = 40usize;
            for _ in 0..MAX_V6_EXT_HEADERS {
                match v6_ext_len(nh, packet, off) {
                    Some(len) => {
                        // A fragment header means the rest may not be here.
                        let is_frag = nh == 44;
                        let next = *packet.get(off)?;
                        off = off.checked_add(len)?;
                        nh = next;
                        if is_frag {
                            // Only the FIRST fragment carries the L4 header, and
                            // we can't tell from here — treat as portless.
                            return Some(L4 {
                                proto: proto_of(nh),
                                dst_port: None,
                            });
                        }
                    }
                    None => break,
                }
            }
            let proto = proto_of(nh);
            Some(L4 {
                proto,
                dst_port: port_at(packet, off, proto),
            })
        }
        _ => None,
    }
}

/// The packet's destination as written on the wire.
///
/// Distinct from [`Router::dst_of_ip_packet`](super::router::Router::dst_of_ip_packet),
/// which *unmaps* a derived-ULA v6 to its embedded v4 because it answers a
/// ROUTING question. Rule matching must see the real address so an admin's v6
/// CIDR matches a v6 packet.
pub fn dst_ip_of(packet: &[u8]) -> Option<IpAddr> {
    match packet.first()? >> 4 {
        4 if packet.len() >= 20 => Some(IpAddr::V4(std::net::Ipv4Addr::new(
            packet[16], packet[17], packet[18], packet[19],
        ))),
        6 if packet.len() >= 40 => {
            let raw: [u8; 16] = packet[24..40].try_into().ok()?;
            Some(IpAddr::V6(std::net::Ipv6Addr::from(raw)))
        }
        _ => None,
    }
}

fn proto_of(n: u8) -> L4Proto {
    match n {
        6 => L4Proto::Tcp,
        17 => L4Proto::Udp,
        other => L4Proto::Other(other),
    }
}

/// Destination port lives at bytes 2..4 of both TCP and UDP headers.
fn port_at(packet: &[u8], off: usize, proto: L4Proto) -> Option<u16> {
    if !matches!(proto, L4Proto::Tcp | L4Proto::Udp) {
        return None;
    }
    let hi = *packet.get(off + 2)?;
    let lo = *packet.get(off + 3)?;
    Some(u16::from_be_bytes([hi, lo]))
}

/// Does a rule constrain ports at all? `1..=65535` is the stored "any" default
/// ([`OverlayRule::all_ports`]), so a rule that never narrowed ports must still
/// match a portless packet (ICMP, a fragment).
fn narrows_ports(rule: &OverlayRule) -> bool {
    rule.port_range.low > 1 || rule.port_range.high < u16::MAX
}

fn proto_matches(rule: &OverlayRule, proto: L4Proto) -> bool {
    match rule.proto {
        ProtocolKind::Any => true,
        ProtocolKind::Tcp => proto == L4Proto::Tcp,
        ProtocolKind::Udp => proto == L4Proto::Udp,
    }
}

fn cidr_contains(cidr: &str, ip: IpAddr) -> bool {
    cidr.trim()
        .parse::<ipnet::IpNet>()
        .map(|n| n.contains(&ip))
        .unwrap_or(false)
}

/// Does `rules` permit a packet to `dst` with this L4 view?
///
/// **First match wins is irrelevant here — these are pure grants**: the server
/// has already collapsed the tenant's policies into the set of rules this peer
/// may use against this node, so any single match is an allow and no match is a
/// deny.
///
/// An **empty** rule set means the server compiled no grant for this peer, which
/// is a DENY under enforcement. Callers must therefore distinguish "no rules
/// shipped at all" (a pre-P4 server / ACL off → don't call this) from "rules
/// shipped and empty" (genuinely denied) — see the `Option` on the peer's
/// stored rules in `wg.rs`.
///
/// A port-narrowed or proto-narrowed rule never matches a packet whose L4 header
/// we couldn't read (`dst_port: None`). That is deliberate and fail-closed: a
/// fragmented packet must not be able to slip past a `:22`-only rule by hiding
/// its port.
pub fn rules_permit(rules: &[OverlayRule], dst: IpAddr, l4: Option<&L4>) -> bool {
    rules.iter().any(|r| {
        if !cidr_contains(&r.cidr, dst) {
            return false;
        }
        let Some(l4) = l4 else {
            // Unparseable packet: only a rule that narrows neither dimension.
            return matches!(r.proto, ProtocolKind::Any) && !narrows_ports(r);
        };
        if !proto_matches(r, l4.proto) {
            return false;
        }
        if !narrows_ports(r) {
            return true;
        }
        match l4.dst_port {
            Some(p) => p >= r.port_range.low && p <= r.port_range.high,
            None => false,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use roomler_ai_remote_control::models::PortRange;

    fn rule(cidr: &str, low: u16, high: u16, proto: ProtocolKind) -> OverlayRule {
        OverlayRule {
            cidr: cidr.into(),
            port_range: PortRange { low, high },
            proto,
        }
    }

    fn any_rule(cidr: &str) -> OverlayRule {
        rule(cidr, 1, u16::MAX, ProtocolKind::Any)
    }

    /// IPv4 with a settable IHL so the options case is covered.
    fn v4(proto: u8, dst_port: u16, ihl_words: u8, frag_off: u16) -> Vec<u8> {
        let ihl = ihl_words as usize * 4;
        let mut p = vec![0u8; ihl + 8];
        p[0] = 0x40 | ihl_words;
        p[6] = ((frag_off >> 8) & 0x1f) as u8;
        p[7] = (frag_off & 0xff) as u8;
        p[9] = proto;
        p[12..16].copy_from_slice(&[100, 64, 0, 1]);
        p[16..20].copy_from_slice(&[10, 66, 51, 147]);
        p[ihl + 2..ihl + 4].copy_from_slice(&dst_port.to_be_bytes());
        p
    }

    #[test]
    fn parses_tcp_and_udp_ports_from_ipv4() {
        let l4 = parse_l4(&v4(6, 22, 5, 0)).unwrap();
        assert_eq!(l4.proto, L4Proto::Tcp);
        assert_eq!(l4.dst_port, Some(22));

        let l4 = parse_l4(&v4(17, 53, 5, 0)).unwrap();
        assert_eq!(l4.proto, L4Proto::Udp);
        assert_eq!(l4.dst_port, Some(53));
    }

    #[test]
    fn honours_ipv4_options_via_ihl() {
        // IHL=7 ⇒ 8 bytes of options; the port must be read past them, not at
        // the fixed offset 20.
        let l4 = parse_l4(&v4(6, 443, 7, 0)).unwrap();
        assert_eq!(l4.dst_port, Some(443));
    }

    #[test]
    fn a_non_first_fragment_has_no_port() {
        let l4 = parse_l4(&v4(6, 22, 5, 185)).unwrap();
        assert_eq!(l4.proto, L4Proto::Tcp);
        assert_eq!(l4.dst_port, None, "a fragment's L4 header isn't here");
    }

    #[test]
    fn icmp_has_a_proto_but_no_port() {
        let l4 = parse_l4(&v4(1, 0, 5, 0)).unwrap();
        assert_eq!(l4.proto, L4Proto::Other(1));
        assert_eq!(l4.dst_port, None);
    }

    #[test]
    fn truncated_and_garbage_packets_never_panic() {
        assert!(parse_l4(&[]).is_none());
        assert!(parse_l4(&[0x45]).is_none());
        assert!(parse_l4(&[0x60; 39]).is_none());
        // IHL smaller than the base header is malformed.
        let mut bad = v4(6, 22, 5, 0);
        bad[0] = 0x44;
        assert!(parse_l4(&bad).is_none());
        // Truncated L4: header claims TCP but the ports aren't present.
        let short = &v4(6, 22, 5, 0)[..21];
        assert_eq!(parse_l4(short).unwrap().dst_port, None);
    }

    #[test]
    fn parses_ipv6_through_a_hop_by_hop_header() {
        let mut p = vec![0u8; 40 + 8 + 8];
        p[0] = 0x60;
        p[6] = 0; // hop-by-hop
        p[40] = 6; // next = TCP
        p[41] = 0; // len 0 ⇒ 8 bytes
        p[48 + 2..48 + 4].copy_from_slice(&8080u16.to_be_bytes());
        let l4 = parse_l4(&p).unwrap();
        assert_eq!(l4.proto, L4Proto::Tcp);
        assert_eq!(l4.dst_port, Some(8080));
    }

    #[test]
    fn a_plain_ipv6_packet_reads_ports_at_40() {
        let mut p = vec![0u8; 48];
        p[0] = 0x60;
        p[6] = 17;
        p[40 + 2..40 + 4].copy_from_slice(&53u16.to_be_bytes());
        let l4 = parse_l4(&p).unwrap();
        assert_eq!(l4.proto, L4Proto::Udp);
        assert_eq!(l4.dst_port, Some(53));
    }

    // -----------------------------------------------------------------------
    // Rule matching
    // -----------------------------------------------------------------------

    fn dst() -> IpAddr {
        "10.66.51.147".parse().unwrap()
    }

    #[test]
    fn an_empty_rule_set_denies() {
        assert!(!rules_permit(&[], dst(), None));
    }

    #[test]
    fn a_cidr_only_rule_permits_any_port_and_proto() {
        let rules = [any_rule("10.66.0.0/16")];
        let tcp = parse_l4(&v4(6, 22, 5, 0)).unwrap();
        assert!(rules_permit(&rules, dst(), Some(&tcp)));
        // …and a portless packet too, since it narrows nothing.
        assert!(rules_permit(&rules, dst(), None));
    }

    #[test]
    fn a_cidr_outside_the_rule_never_matches() {
        let rules = [any_rule("10.66.0.0/16")];
        let outside: IpAddr = "10.84.6.5".parse().unwrap();
        let tcp = parse_l4(&v4(6, 22, 5, 0)).unwrap();
        assert!(!rules_permit(&rules, outside, Some(&tcp)));
    }

    #[test]
    fn a_port_narrowed_rule_admits_only_that_port() {
        let rules = [rule("10.66.0.0/16", 22, 22, ProtocolKind::Any)];
        let ssh = parse_l4(&v4(6, 22, 5, 0)).unwrap();
        let https = parse_l4(&v4(6, 443, 5, 0)).unwrap();
        assert!(rules_permit(&rules, dst(), Some(&ssh)));
        assert!(!rules_permit(&rules, dst(), Some(&https)));
    }

    #[test]
    fn a_port_narrowed_rule_rejects_a_packet_hiding_its_port() {
        // The fail-closed case that matters: a fragment must not slip past a
        // :22-only rule just because its port isn't readable.
        let rules = [rule("10.66.0.0/16", 22, 22, ProtocolKind::Any)];
        let frag = parse_l4(&v4(6, 22, 5, 185)).unwrap();
        assert!(!rules_permit(&rules, dst(), Some(&frag)));
        assert!(!rules_permit(&rules, dst(), None));
    }

    #[test]
    fn proto_narrowing_discriminates() {
        let tcp_only = [rule("10.66.0.0/16", 1, u16::MAX, ProtocolKind::Tcp)];
        let tcp = parse_l4(&v4(6, 22, 5, 0)).unwrap();
        let udp = parse_l4(&v4(17, 22, 5, 0)).unwrap();
        let icmp = parse_l4(&v4(1, 0, 5, 0)).unwrap();
        assert!(rules_permit(&tcp_only, dst(), Some(&tcp)));
        assert!(!rules_permit(&tcp_only, dst(), Some(&udp)));
        assert!(!rules_permit(&tcp_only, dst(), Some(&icmp)));
    }

    #[test]
    fn any_matching_rule_grants_regardless_of_order() {
        // These are grants, not a first-match-wins chain: a later rule can
        // widen what an earlier one didn't cover.
        let rules = [
            rule("10.66.0.0/16", 22, 22, ProtocolKind::Tcp),
            rule("10.66.0.0/16", 443, 443, ProtocolKind::Tcp),
        ];
        let https = parse_l4(&v4(6, 443, 5, 0)).unwrap();
        assert!(rules_permit(&rules, dst(), Some(&https)));
    }

    #[test]
    fn v6_destinations_match_v6_cidrs() {
        let rules = [any_rule("fd72:6f6f:6d6c::/96")];
        let d: IpAddr = "fd72:6f6f:6d6c::6440:7".parse().unwrap();
        assert!(rules_permit(&rules, d, None));
        let outside: IpAddr = "2606:4700::1111".parse().unwrap();
        assert!(!rules_permit(&rules, outside, None));
    }

    #[test]
    fn a_malformed_cidr_never_grants() {
        let rules = [any_rule("not-a-cidr")];
        assert!(!rules_permit(&rules, dst(), None));
    }
}

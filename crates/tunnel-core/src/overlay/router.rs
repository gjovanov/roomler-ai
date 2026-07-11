//! Overlay crypto-routing table.
//!
//! boringtun's [`Tunn`](boringtun::noise::Tunn) is **single-peer** — it
//! has no notion of `allowed_ips`. The mesh's routing therefore lives
//! here: a map from a peer's overlay address to its WireGuard public
//! key. Because every overlay peer is advertised with a single-host
//! `allowed_ips` (its `overlay_ip/32`), this is an exact-match
//! `HashMap<Ipv4Addr, [u8; 32]>` rather than a longest-prefix trie.
//!
//! Outbound path: read the destination address out of the IP packet
//! header → [`Router::route`] → the peer's pubkey → the peer's `Tunn`.
//!
//! **IPv6 (dual-stack)**: a node's overlay v6 is *derived* from its overlay
//! v4 — the v4 embedded in the low 32 bits of Roomler's ULA `/96`
//! ([`derive_overlay_v6`], `docs/netstack-ipv6-plan.md`). Routing therefore
//! needs **no v6 table**: [`Router::dst_of_ip_packet`] unmaps a derived-ULA
//! destination back to its embedded v4 and routes on the existing v4 entries,
//! covering every present and future peer with zero per-peer state and zero
//! netmap change. A v6 destination outside the ULA (link-local/multicast OS
//! noise, genuine internet v6) is unroutable by construction and dropped.

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};

/// Roomler's overlay IPv6 ULA — `fd72:6f6f:6d6c::/48` (`fd` + ASCII "rooml").
/// A node's overlay v6 is its overlay v4 embedded in the low 32 bits of the
/// fixed `/96` inside this ULA, so v6 is *derived*, never separately allocated
/// (see `docs/netstack-ipv6-plan.md`). Pinned once — every derived address
/// bakes it in, so it must never change.
const OVERLAY_ULA_HEXTETS: [u16; 6] = [0xfd72, 0x6f6f, 0x6d6c, 0, 0, 0];
/// On-link prefix for the derived-v6 network: the fixed `/96` (48-bit ULA + 48
/// zero bits). Assigning an iface this prefix makes every peer's
/// `fd72:6f6f:6d6c::<their-v4>` on-link — the v6 mirror of how the v4 network
/// prefix makes peers on-link — so no OS/netstack route table is needed for
/// peer traffic.
pub const OVERLAY_V6_ONLINK_PREFIX: u8 = 96;

/// Derive a node's overlay IPv6 from its overlay IPv4: embed the 32-bit v4 in
/// the low 32 bits of Roomler's ULA `/96` (`fd72:6f6f:6d6c::<v4>`). Deterministic
/// and reversible ([`embedded_v4_of_overlay_v6`]), so a node self-derives its
/// own v6 and every peer's v6 from the v4 the server already assigns — no
/// server allocation, no wire change.
pub fn derive_overlay_v6(v4: Ipv4Addr) -> Ipv6Addr {
    let o = v4.octets();
    let [a, b, c, d, e, f] = OVERLAY_ULA_HEXTETS;
    Ipv6Addr::new(
        a,
        b,
        c,
        d,
        e,
        f,
        u16::from_be_bytes([o[0], o[1]]),
        u16::from_be_bytes([o[2], o[3]]),
    )
}

/// The inverse of [`derive_overlay_v6`]: the overlay v4 embedded in a
/// derived-ULA v6, or `None` if `v6` is not inside Roomler's ULA `/96`.
pub fn embedded_v4_of_overlay_v6(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let seg = v6.segments();
    if seg[..6] != OVERLAY_ULA_HEXTETS {
        return None;
    }
    let hi = seg[6].to_be_bytes();
    let lo = seg[7].to_be_bytes();
    Some(Ipv4Addr::new(hi[0], hi[1], lo[0], lo[1]))
}

/// An IPv4 CIDR — Phase 1 subnet routes. Hand-rolled (no dep): the overlay only
/// needs `contains` + `parse` for a handful of advertised subnets per peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    /// Network address, already masked to `prefix` bits.
    base: u32,
    prefix: u8,
}

impl Cidr {
    /// Parse `"a.b.c.d/nn"`. `None` on malformed input or `nn > 32`.
    pub fn parse(s: &str) -> Option<Self> {
        let (ip, pfx) = s.split_once('/')?;
        let ip: Ipv4Addr = ip.parse().ok()?;
        let prefix: u8 = pfx.parse().ok()?;
        if prefix > 32 {
            return None;
        }
        let mask = Self::mask(prefix);
        Some(Self {
            base: u32::from(ip) & mask,
            prefix,
        })
    }

    fn mask(prefix: u8) -> u32 {
        if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - prefix)
        }
    }

    /// Does this CIDR contain `ip`?
    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        (u32::from(ip) & Self::mask(self.prefix)) == self.base
    }
}

impl std::fmt::Display for Cidr {
    /// Canonical `network/prefix` (the base is already masked), e.g.
    /// `"192.168.1.0/24"` — used to hand the route to the OS.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", Ipv4Addr::from(self.base), self.prefix)
    }
}

/// Overlay crypto-routing table: exact-match `/32` host routes (the fast path
/// every peer carries) plus optional per-peer subnet CIDRs (Phase 1 subnet
/// router), resolved by longest-prefix on a host-route miss.
#[derive(Debug, Default, Clone)]
pub struct Router {
    by_ip: HashMap<Ipv4Addr, [u8; 32]>,
    subnets: Vec<(Cidr, [u8; 32])>,
}

impl Router {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install / replace the host route for `ip` → `pubkey`.
    pub fn upsert(&mut self, ip: Ipv4Addr, pubkey: [u8; 32]) {
        self.by_ip.insert(ip, pubkey);
    }

    /// Phase 1 — replace `pubkey`'s advertised subnet routes (empty clears them).
    pub fn set_subnets(&mut self, pubkey: [u8; 32], cidrs: &[Cidr]) {
        self.subnets.retain(|(_, pk)| *pk != pubkey);
        self.subnets.extend(cidrs.iter().map(|c| (*c, pubkey)));
    }

    /// Drop the `/32` route for `ip` AND any subnet routes owned by the same
    /// peer; returns the pubkey the host route pointed at (if any) so the caller
    /// can also drop the matching `Tunn`.
    pub fn remove(&mut self, ip: &Ipv4Addr) -> Option<[u8; 32]> {
        let pk = self.by_ip.remove(ip);
        if let Some(pk) = pk {
            self.subnets.retain(|(_, p)| *p != pk);
        }
        pk
    }

    /// Which peer owns the overlay destination `ip`? Exact `/32` first, else the
    /// longest-prefix subnet route that contains it.
    pub fn route(&self, ip: &Ipv4Addr) -> Option<[u8; 32]> {
        if let Some(pk) = self.by_ip.get(ip) {
            return Some(*pk);
        }
        self.subnets
            .iter()
            .filter(|(c, _)| c.contains(*ip))
            .max_by_key(|(c, _)| c.prefix)
            .map(|(_, pk)| *pk)
    }

    /// The **v4 routing key** for a raw IP packet's destination, or `None` if
    /// it is unroutable. The TUN/netstack bridge hands raw IP packets here to
    /// pick a peer:
    /// * IPv4 → the dst field (bytes 16..20).
    /// * IPv6 → the dst field (bytes 24..40 of the fixed header) **unmapped**
    ///   to its embedded v4 iff it is a derived-ULA address
    ///   ([`embedded_v4_of_overlay_v6`]) — so v6 routes on the same v4 table.
    ///   Any other v6 (link-local/multicast OS noise, internet v6) → `None`.
    pub fn dst_of_ip_packet(packet: &[u8]) -> Option<Ipv4Addr> {
        match packet.first()? >> 4 {
            4 if packet.len() >= 20 => Some(Ipv4Addr::new(
                packet[16], packet[17], packet[18], packet[19],
            )),
            6 if packet.len() >= 40 => {
                let dst: [u8; 16] = packet[24..40].try_into().ok()?;
                embedded_v4_of_overlay_v6(Ipv6Addr::from(dst))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_round_trips() {
        let mut r = Router::new();
        let ip = Ipv4Addr::new(100, 64, 0, 7);
        let key = [9u8; 32];
        r.upsert(ip, key);
        assert_eq!(r.route(&ip), Some(key));
        assert_eq!(r.route(&Ipv4Addr::new(100, 64, 0, 8)), None);
        assert_eq!(r.remove(&ip), Some(key));
        assert_eq!(r.route(&ip), None);
    }

    #[test]
    fn subnet_routes_longest_prefix_and_removal() {
        let mut r = Router::new();
        let gw = [1u8; 32];
        let other = [2u8; 32];
        r.upsert(Ipv4Addr::new(100, 64, 0, 1), gw); // gw's own /32
        r.upsert(Ipv4Addr::new(100, 64, 0, 2), other);
        r.set_subnets(gw, &[Cidr::parse("192.168.0.0/16").unwrap()]);
        r.set_subnets(other, &[Cidr::parse("192.168.1.0/24").unwrap()]);

        // Exact host route wins.
        assert_eq!(r.route(&Ipv4Addr::new(100, 64, 0, 1)), Some(gw));
        // /16 catches 192.168.2.5.
        assert_eq!(r.route(&Ipv4Addr::new(192, 168, 2, 5)), Some(gw));
        // Longest-prefix: 192.168.1.9 → other (/24 beats /16).
        assert_eq!(r.route(&Ipv4Addr::new(192, 168, 1, 9)), Some(other));
        // Unknown → None.
        assert_eq!(r.route(&Ipv4Addr::new(10, 0, 0, 1)), None);

        // Removing gw's /32 also drops its /16; other's /24 survives.
        assert_eq!(r.remove(&Ipv4Addr::new(100, 64, 0, 1)), Some(gw));
        assert_eq!(r.route(&Ipv4Addr::new(192, 168, 2, 5)), None);
        assert_eq!(r.route(&Ipv4Addr::new(192, 168, 1, 9)), Some(other));
    }

    #[test]
    fn cidr_parse_and_contains() {
        let c = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(c.contains(Ipv4Addr::new(10, 5, 6, 7)));
        assert!(!c.contains(Ipv4Addr::new(11, 0, 0, 1)));
        assert!(
            Cidr::parse("0.0.0.0/0")
                .unwrap()
                .contains(Ipv4Addr::new(8, 8, 8, 8))
        );
        assert!(Cidr::parse("bad").is_none());
        assert!(Cidr::parse("1.2.3.4/33").is_none());
    }

    #[test]
    fn dst_of_ip_packet_reads_v4_header() {
        // Minimal IPv4 header: version/IHL=0x45, then 12 bytes, then
        // src (4) at offset 12, dst (4) at offset 16.
        let mut pkt = [0u8; 20];
        pkt[0] = 0x45;
        pkt[16..20].copy_from_slice(&[100, 64, 0, 9]);
        assert_eq!(
            Router::dst_of_ip_packet(&pkt),
            Some(Ipv4Addr::new(100, 64, 0, 9))
        );
        // Short / empty buffers reject.
        assert_eq!(Router::dst_of_ip_packet(&[0u8; 10]), None);
        assert_eq!(Router::dst_of_ip_packet(&[]), None);
        // A version nibble that is neither 4 nor 6 rejects.
        let mut junk = [0u8; 40];
        junk[0] = 0x50;
        assert_eq!(Router::dst_of_ip_packet(&junk), None);
    }

    #[test]
    fn derive_and_unmap_overlay_v6_round_trip() {
        let v4 = Ipv4Addr::new(100, 64, 3, 129);
        let v6 = derive_overlay_v6(v4);
        // The textual form pins the ULA scheme (fd72:6f6f:6d6c::/96).
        assert_eq!(v6.to_string(), "fd72:6f6f:6d6c::6440:381");
        assert_eq!(embedded_v4_of_overlay_v6(v6), Some(v4));
        // Outside the ULA → not an overlay v6.
        assert_eq!(embedded_v4_of_overlay_v6("fe80::1".parse().unwrap()), None);
        assert_eq!(
            embedded_v4_of_overlay_v6("fd00::6440:381".parse().unwrap()),
            None
        );
    }

    #[test]
    fn dst_of_ip_packet_unmaps_derived_v6_and_drops_other_v6() {
        let v4 = Ipv4Addr::new(100, 64, 0, 9);
        // Minimal IPv6 fixed header: version nibble 6; dst at bytes 24..40.
        let mut pkt = [0u8; 40];
        pkt[0] = 0x60;
        pkt[24..40].copy_from_slice(&derive_overlay_v6(v4).octets());
        assert_eq!(Router::dst_of_ip_packet(&pkt), Some(v4));

        // A non-ULA v6 destination (OS link-local noise) is unroutable.
        let mut ll = [0u8; 40];
        ll[0] = 0x60;
        ll[24..40].copy_from_slice(&"fe80::1".parse::<Ipv6Addr>().unwrap().octets());
        assert_eq!(Router::dst_of_ip_packet(&ll), None);

        // A truncated v6 header rejects.
        assert_eq!(Router::dst_of_ip_packet(&[0x60; 39]), None);
    }
}

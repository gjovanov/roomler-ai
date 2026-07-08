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

use std::collections::HashMap;
use std::net::Ipv4Addr;

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

    /// Destination address from an IPv4 packet (bytes 16..20), or `None`
    /// for a non-IPv4 / too-short buffer. The TUN bridge (Phase 3) hands
    /// raw IP packets here to pick a peer.
    pub fn dst_of_ipv4_packet(packet: &[u8]) -> Option<Ipv4Addr> {
        // Version nibble must be 4 and the header must reach the dst
        // field.
        if packet.len() < 20 || (packet[0] >> 4) != 4 {
            return None;
        }
        Some(Ipv4Addr::new(
            packet[16], packet[17], packet[18], packet[19],
        ))
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
    fn dst_of_ipv4_packet_reads_header() {
        // Minimal IPv4 header: version/IHL=0x45, then 12 bytes, then
        // src (4) at offset 12, dst (4) at offset 16.
        let mut pkt = [0u8; 20];
        pkt[0] = 0x45;
        pkt[16..20].copy_from_slice(&[100, 64, 0, 9]);
        assert_eq!(
            Router::dst_of_ipv4_packet(&pkt),
            Some(Ipv4Addr::new(100, 64, 0, 9))
        );
        // Non-IPv4 / short buffers reject.
        assert_eq!(Router::dst_of_ipv4_packet(&[0u8; 10]), None);
        let mut v6 = [0u8; 20];
        v6[0] = 0x60;
        assert_eq!(Router::dst_of_ipv4_packet(&v6), None);
    }
}

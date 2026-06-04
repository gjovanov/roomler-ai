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

/// Exact-match overlay routing table (`overlay_ip → wg_public_key`).
#[derive(Debug, Default, Clone)]
pub struct Router {
    by_ip: HashMap<Ipv4Addr, [u8; 32]>,
}

impl Router {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install / replace the route for `ip` → `pubkey`.
    pub fn upsert(&mut self, ip: Ipv4Addr, pubkey: [u8; 32]) {
        self.by_ip.insert(ip, pubkey);
    }

    /// Drop the route for `ip`, returning the pubkey it pointed at (if
    /// any) so the caller can also drop the matching `Tunn`.
    pub fn remove(&mut self, ip: &Ipv4Addr) -> Option<[u8; 32]> {
        self.by_ip.remove(ip)
    }

    /// Which peer owns the overlay destination `ip`?
    pub fn route(&self, ip: &Ipv4Addr) -> Option<[u8; 32]> {
        self.by_ip.get(ip).copied()
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

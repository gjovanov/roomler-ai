//! Multi-org egress source normalization — the pure half of the mux NAT.
//!
//! On a multi-org host every org's overlay address lives on the ONE shared
//! `roomler` adapter, and org blocks may legitimately nest (a legacy `/10`
//! tenant beside carved `/22`s — see `tun_mux`). The OS picks the source
//! address for locally-originated traffic, and with nested prefixes it can
//! (and on Windows deterministically does — field 2026-08-09, CORPLAP-1) pick
//! the WRONG org's address for an overlay destination. The packet then rides
//! the correct pair with a foreign source: single-org receivers reply to an
//! address they cannot route and the flow fails silently with 100 % loss.
//!
//! The fix is a tiny, host-local NAT at the mux boundary:
//! * egress (`tun_mux::route_inbound`): a host-originated v4 packet whose
//!   source is ANOTHER org's own address is rewritten to the winning org's
//!   address, and the flow is recorded here;
//! * ingress (`tun_mux::MuxPort::write_packet`): a reply addressed to the
//!   winning org's address that matches a recorded flow gets its destination
//!   restored, so the OS delivers it to the socket that is still anchored to
//!   the address the OS originally chose.
//!
//! This module is pure and platform-agnostic: the flow table, the v4 header
//! view, and the RFC 1624 incremental checksum rewrites. The hooks, the gate
//! and all logging live in `tun_mux`. Full background: `docs/multi-org.md`.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

/// Flow-table capacity. The table is populated only by the host's OWN
/// cross-org traffic (the trigger is `src == another org's self address`,
/// which forwarded traffic can never carry), so pressure here means a very
/// chatty local workload, not an attack surface.
pub(crate) const FLOW_CAP: usize = 4096;
/// Idle TTL for TCP flows. Generous: a fully idle TCP session whose PEER
/// sends first after expiry would have its packet delivered to the wire
/// address and answered with an RST from the wrong identity.
const FLOW_TTL_TCP: Duration = Duration::from_secs(600);
/// Idle TTL for everything else (UDP, ICMP echo).
const FLOW_TTL_OTHER: Duration = Duration::from_secs(120);

pub(crate) const PROTO_ICMP: u8 = 1;
pub(crate) const PROTO_TCP: u8 = 6;
pub(crate) const PROTO_UDP: u8 = 17;

/// One normalized egress flow, keyed from this host's perspective.
/// For ICMP echo the identifier plays the `local_port` role and
/// `remote_port` is 0.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct FlowKey {
    pub proto: u8,
    pub remote: Ipv4Addr,
    pub remote_port: u16,
    pub local_port: u16,
}

pub(crate) struct FlowVal {
    /// The (wrong-org) source the OS chose — what ingress restores.
    pub orig_src: Ipv4Addr,
    /// What egress rewrote the source TO (the owning org's address). Checked
    /// on the reverse match so entries go inert if an org re-registers with
    /// a different address.
    pub wire_src: Ipv4Addr,
    pub last_used: Instant,
}

fn ttl_for(proto: u8) -> Duration {
    if proto == PROTO_TCP {
        FLOW_TTL_TCP
    } else {
        FLOW_TTL_OTHER
    }
}

/// The normalized-flow table. Hand-rolled bookkeeping (no dep), same stance
/// as `router::Cidr`.
#[derive(Default)]
pub(crate) struct FlowMap {
    map: HashMap<FlowKey, FlowVal>,
}

impl FlowMap {
    /// Insert or refresh the egress side of a flow. Returns `false` when the
    /// table is full even after an expiry sweep — the caller must then pass
    /// TCP/UDP through UNrewritten (a rewritten-but-unmapped flow fails with
    /// zero diagnostics; an unrewritten one keeps today's behavior plus the
    /// receiver-side RPF breadcrumb). Deliberately never evicts a live entry:
    /// sacrificing an ESTABLISHED mapping mid-stream is strictly worse than
    /// degrading a NEW flow.
    pub fn note_egress(
        &mut self,
        key: FlowKey,
        orig_src: Ipv4Addr,
        wire_src: Ipv4Addr,
        now: Instant,
    ) -> bool {
        if let Some(v) = self.map.get_mut(&key) {
            v.orig_src = orig_src;
            v.wire_src = wire_src;
            v.last_used = now;
            return true;
        }
        if self.map.len() >= FLOW_CAP {
            self.map
                .retain(|k, v| now.saturating_duration_since(v.last_used) <= ttl_for(k.proto));
            if self.map.len() >= FLOW_CAP {
                return false;
            }
        }
        self.map.insert(
            key,
            FlowVal {
                orig_src,
                wire_src,
                last_used: now,
            },
        );
        true
    }

    /// Reverse lookup for an inbound packet addressed to `wire_src`. A hit
    /// refreshes the entry (both directions keep a flow alive); an expired or
    /// re-pointed entry is a miss.
    pub fn restore_dst(
        &mut self,
        key: &FlowKey,
        wire_src: Ipv4Addr,
        now: Instant,
    ) -> Option<Ipv4Addr> {
        let v = self.map.get_mut(key)?;
        if v.wire_src != wire_src {
            return None;
        }
        if now.saturating_duration_since(v.last_used) > ttl_for(key.proto) {
            self.map.remove(key);
            return None;
        }
        v.last_used = now;
        Some(v.orig_src)
    }

    /// Drop every flow that references `addr` as either side's local address.
    /// Called when an org deregisters — its OS address is going away, so both
    /// restoring TO it and rewriting FROM it are pointless.
    pub fn purge_addr(&mut self, addr: Ipv4Addr) {
        self.map
            .retain(|_, v| v.orig_src != addr && v.wire_src != addr);
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

/// A length-checked view of an IPv4 packet's fixed facts. `None` for
/// anything that is not a well-formed v4 header (v6, truncated, IHL < 20).
#[derive(Clone, Copy, Debug)]
pub(crate) struct V4View {
    pub ihl: usize,
    pub proto: u8,
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    /// Part of a fragment train (MF set OR a nonzero offset).
    pub fragment: bool,
    /// Offset 0 — the (only) fragment carrying the L4 header.
    pub first_fragment: bool,
}

pub(crate) fn v4_view(pkt: &[u8]) -> Option<V4View> {
    if pkt.len() < 20 || pkt[0] >> 4 != 4 {
        return None;
    }
    let ihl = usize::from(pkt[0] & 0x0F) * 4;
    if ihl < 20 || pkt.len() < ihl {
        return None;
    }
    let frag = u16::from_be_bytes([pkt[6], pkt[7]]);
    let mf = frag & 0x2000 != 0;
    let offset = frag & 0x1FFF;
    Some(V4View {
        ihl,
        proto: pkt[9],
        src: Ipv4Addr::new(pkt[12], pkt[13], pkt[14], pkt[15]),
        dst: Ipv4Addr::new(pkt[16], pkt[17], pkt[18], pkt[19]),
        fragment: mf || offset != 0,
        first_fragment: offset == 0,
    })
}

fn be16(pkt: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*pkt.get(at)?, *pkt.get(at + 1)?]))
}

/// ICMP echo request / reply type bytes.
const ICMP_ECHO_REQUEST: u8 = 8;
const ICMP_ECHO_REPLY: u8 = 0;

/// Flow key of an OUTBOUND offset-0 packet. `None` for a truncated L4
/// header, a non-echo-request ICMP message, or an unsupported protocol —
/// callers treat `None` as "cannot be tracked".
pub(crate) fn egress_key(pkt: &[u8], v: &V4View) -> Option<FlowKey> {
    match v.proto {
        PROTO_TCP | PROTO_UDP => Some(FlowKey {
            proto: v.proto,
            remote: v.dst,
            remote_port: be16(pkt, v.ihl + 2)?,
            local_port: be16(pkt, v.ihl)?,
        }),
        PROTO_ICMP => {
            if *pkt.get(v.ihl)? != ICMP_ECHO_REQUEST {
                return None;
            }
            Some(FlowKey {
                proto: PROTO_ICMP,
                remote: v.dst,
                remote_port: 0,
                local_port: be16(pkt, v.ihl + 4)?,
            })
        }
        _ => None,
    }
}

/// Flow key of an INBOUND (reply-side) offset-0 packet: port roles swapped
/// relative to [`egress_key`], so the reply to a recorded egress produces the
/// SAME key. ICMP: only an echo REPLY keys — an inbound echo REQUEST from the
/// peer must never match an entry created by our own outbound request.
pub(crate) fn ingress_key(pkt: &[u8], v: &V4View) -> Option<FlowKey> {
    match v.proto {
        PROTO_TCP | PROTO_UDP => Some(FlowKey {
            proto: v.proto,
            remote: v.src,
            remote_port: be16(pkt, v.ihl)?,
            local_port: be16(pkt, v.ihl + 2)?,
        }),
        PROTO_ICMP => {
            if *pkt.get(v.ihl)? != ICMP_ECHO_REPLY {
                return None;
            }
            Some(FlowKey {
                proto: PROTO_ICMP,
                remote: v.src,
                remote_port: 0,
                local_port: be16(pkt, v.ihl + 4)?,
            })
        }
        _ => None,
    }
}

/// RFC 1624 eqn. 3 — incremental one's-complement checksum update for one
/// 32-bit field changing `old` → `new`: `HC' = ~(~HC + ~m + m')`.
fn cksum_update32(cksum: u16, old: u32, new: u32) -> u16 {
    let mut sum: u32 = u32::from(!cksum);
    for w in [(old >> 16) as u16, old as u16] {
        sum += u32::from(!w);
    }
    for w in [(new >> 16) as u16, new as u16] {
        sum += u32::from(w);
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Patch the 16-bit checksum field at `at` for a 32-bit address change. For
/// UDP a stored 0 means "no checksum" and is left alone, and a recomputed 0
/// is transmitted as 0xFFFF (RFC 768).
fn patch_cksum(pkt: &mut [u8], at: usize, old: u32, new: u32, udp: bool) {
    if pkt.len() < at + 2 {
        return;
    }
    let stored = u16::from_be_bytes([pkt[at], pkt[at + 1]]);
    if udp && stored == 0 {
        return;
    }
    let mut updated = cksum_update32(stored, old, new);
    if udp && updated == 0 {
        updated = 0xFFFF;
    }
    pkt[at..at + 2].copy_from_slice(&updated.to_be_bytes());
}

/// Rewrite the L4 checksum for an address change, when the L4 header is
/// present (offset-0). TCP/UDP checksums cover the pseudo-header (both
/// addresses); ICMPv4's does not and stays untouched.
fn patch_l4(pkt: &mut [u8], v: &V4View, old: u32, new: u32) {
    if !v.first_fragment {
        return;
    }
    match v.proto {
        PROTO_TCP => patch_cksum(pkt, v.ihl + 16, old, new, false),
        PROTO_UDP => patch_cksum(pkt, v.ihl + 6, old, new, true),
        _ => {}
    }
}

/// In-place source rewrite: address bytes, IPv4 header checksum, and (on an
/// offset-0 packet) the TCP/UDP pseudo-header checksum.
pub(crate) fn rewrite_src(pkt: &mut [u8], v: &V4View, new_src: Ipv4Addr) {
    let old = u32::from(v.src);
    let new = u32::from(new_src);
    pkt[12..16].copy_from_slice(&new_src.octets());
    patch_cksum(pkt, 10, old, new, false);
    patch_l4(pkt, v, old, new);
}

/// In-place destination rewrite — the ingress twin of [`rewrite_src`].
pub(crate) fn rewrite_dst(pkt: &mut [u8], v: &V4View, new_dst: Ipv4Addr) {
    let old = u32::from(v.dst);
    let new = u32::from(new_dst);
    pkt[16..20].copy_from_slice(&new_dst.octets());
    patch_cksum(pkt, 10, old, new, false);
    patch_l4(pkt, v, old, new);
}

/// Reference implementations for the tests: full one's-complement recompute,
/// asserted against the incremental updates above.
#[cfg(test)]
pub(crate) mod reference {
    use super::*;

    fn ones_sum(words: impl Iterator<Item = u16>) -> u32 {
        let mut sum = 0u32;
        for w in words {
            sum += u32::from(w);
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        sum
    }

    fn words(bytes: &[u8]) -> Vec<u16> {
        bytes
            .chunks(2)
            .map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]))
            .collect()
    }

    /// Full IPv4 header checksum (checksum field treated as zero).
    pub fn ipv4_header_cksum(pkt: &[u8]) -> u16 {
        let ihl = usize::from(pkt[0] & 0x0F) * 4;
        let mut hdr = pkt[..ihl].to_vec();
        hdr[10] = 0;
        hdr[11] = 0;
        !(ones_sum(words(&hdr).into_iter()) as u16)
    }

    /// Full L4 checksum: pseudo-header + segment for TCP/UDP, plain segment
    /// for ICMP. `None` for other protocols.
    pub fn l4_cksum(pkt: &[u8], v: &V4View) -> Option<u16> {
        let seg = &pkt[v.ihl..];
        let cksum_at = match v.proto {
            PROTO_TCP => 16,
            PROTO_UDP => 6,
            PROTO_ICMP => 2,
            _ => return None,
        };
        let mut seg = seg.to_vec();
        seg[cksum_at] = 0;
        seg[cksum_at + 1] = 0;
        let mut sum = ones_sum(words(&seg).into_iter());
        if v.proto != PROTO_ICMP {
            let pseudo: Vec<u16> = words(&v.src.octets())
                .into_iter()
                .chain(words(&v.dst.octets()))
                .chain([u16::from(v.proto), seg.len() as u16])
                .collect();
            sum += ones_sum(pseudo.into_iter());
            while sum >> 16 != 0 {
                sum = (sum & 0xFFFF) + (sum >> 16);
            }
        }
        Some(!(sum as u16))
    }

    /// Build a well-formed v4 packet with correct checksums. `l4` starts at
    /// the L4 header (20-byte IHL). Shared with `tun_mux`'s integration tests.
    pub fn mk(proto: u8, src: Ipv4Addr, dst: Ipv4Addr, l4: &[u8]) -> Vec<u8> {
        let total = 20 + l4.len();
        let mut p = vec![0u8; total];
        p[0] = 0x45;
        p[2..4].copy_from_slice(&(total as u16).to_be_bytes());
        p[8] = 64;
        p[9] = proto;
        p[12..16].copy_from_slice(&src.octets());
        p[16..20].copy_from_slice(&dst.octets());
        p[20..].copy_from_slice(l4);
        let v = v4_view(&p).unwrap();
        let ip_ck = ipv4_header_cksum(&p);
        p[10..12].copy_from_slice(&ip_ck.to_be_bytes());
        if let Some(l4_ck) = l4_cksum(&p, &v) {
            let at = v.ihl
                + match proto {
                    PROTO_TCP => 16,
                    PROTO_UDP => 6,
                    _ => 2,
                };
            p[at..at + 2].copy_from_slice(&l4_ck.to_be_bytes());
        }
        p
    }

    pub fn mk_udp(src: Ipv4Addr, dst: Ipv4Addr, sport: u16, dport: u16) -> Vec<u8> {
        let mut l4 = vec![0u8; 12];
        l4[0..2].copy_from_slice(&sport.to_be_bytes());
        l4[2..4].copy_from_slice(&dport.to_be_bytes());
        l4[4..6].copy_from_slice(&12u16.to_be_bytes());
        l4[8..].copy_from_slice(b"ping");
        mk(PROTO_UDP, src, dst, &l4)
    }

    pub fn mk_tcp(src: Ipv4Addr, dst: Ipv4Addr, sport: u16, dport: u16) -> Vec<u8> {
        let mut l4 = vec![0u8; 20];
        l4[0..2].copy_from_slice(&sport.to_be_bytes());
        l4[2..4].copy_from_slice(&dport.to_be_bytes());
        l4[12] = 0x50;
        l4[13] = 0x10;
        mk(PROTO_TCP, src, dst, &l4)
    }

    pub fn mk_icmp(src: Ipv4Addr, dst: Ipv4Addr, icmp_type: u8, id: u16) -> Vec<u8> {
        let mut l4 = vec![0u8; 12];
        l4[0] = icmp_type;
        l4[4..6].copy_from_slice(&id.to_be_bytes());
        l4[6..8].copy_from_slice(&7u16.to_be_bytes());
        l4[8..].copy_from_slice(b"data");
        mk(PROTO_ICMP, src, dst, &l4)
    }

    /// Assert stored checksums equal a full recompute (UDP's 0/0xFFFF
    /// equivalence honored).
    pub fn assert_checksums_valid(pkt: &[u8]) {
        let v = v4_view(pkt).unwrap();
        let stored_ip = u16::from_be_bytes([pkt[10], pkt[11]]);
        assert_eq!(stored_ip, ipv4_header_cksum(pkt), "ip header cksum");
        if let Some(want) = l4_cksum(pkt, &v) {
            let at = v.ihl
                + match v.proto {
                    PROTO_TCP => 16,
                    PROTO_UDP => 6,
                    _ => 2,
                };
            let got = u16::from_be_bytes([pkt[at], pkt[at + 1]]);
            // UDP transmits a recomputed 0 as 0xFFFF; both validate.
            if v.proto == PROTO_UDP && want == 0 {
                assert!(got == 0 || got == 0xFFFF, "udp cksum {got:#06x}");
            } else {
                assert_eq!(got, want, "l4 cksum proto={}", v.proto);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::reference::{assert_checksums_valid, mk_icmp, mk_tcp, mk_udp};
    use super::*;

    /// Deterministic LCG so the checksum matrix is reproducible.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 32) as u32
        }
    }

    fn ip(n: u32) -> Ipv4Addr {
        Ipv4Addr::from(n)
    }

    #[test]
    fn cksum_update32_matches_full_recompute() {
        let mut lcg = Lcg(0x5eed);
        let mut pkt = mk_udp(ip(0x64400005), ip(0x64400002), 40000, 53);
        // Boundary values first, then a random walk — every step re-validated
        // against the full recompute.
        let mut srcs: Vec<u32> = vec![0x00000000, 0xFFFFFFFF, 0x64400c1c];
        srcs.extend((0..64).map(|_| lcg.next()));
        for s in srcs {
            let v = v4_view(&pkt).unwrap();
            rewrite_src(&mut pkt, &v, ip(s));
            assert_checksums_valid(&pkt);
        }
        // And the dst twin.
        let mut dsts: Vec<u32> = vec![0x00000000, 0xFFFFFFFF];
        dsts.extend((0..64).map(|_| lcg.next()));
        for d in dsts {
            let v = v4_view(&pkt).unwrap();
            rewrite_dst(&mut pkt, &v, ip(d));
            assert_checksums_valid(&pkt);
        }
    }

    #[test]
    fn rewrite_src_fixes_ip_and_l4_checksums() {
        for mut pkt in [
            mk_tcp(ip(0x64410005), ip(0x64400002), 50000, 22),
            mk_udp(ip(0x64410005), ip(0x64400002), 50000, 53),
            mk_icmp(ip(0x64410005), ip(0x64400002), 8, 7),
        ] {
            let v = v4_view(&pkt).unwrap();
            let icmp_ck_before = if v.proto == PROTO_ICMP {
                Some([pkt[v.ihl + 2], pkt[v.ihl + 3]])
            } else {
                None
            };
            rewrite_src(&mut pkt, &v, ip(0x6440001c));
            assert_eq!(v4_view(&pkt).unwrap().src, ip(0x6440001c));
            assert_checksums_valid(&pkt);
            if let Some(before) = icmp_ck_before {
                let v = v4_view(&pkt).unwrap();
                assert_eq!(
                    [pkt[v.ihl + 2], pkt[v.ihl + 3]],
                    before,
                    "ICMP checksum has no pseudo-header and must not change"
                );
            }
        }
    }

    #[test]
    fn udp_zero_checksum_is_left_alone_and_never_folds_to_zero() {
        let mut pkt = mk_udp(ip(0x64410005), ip(0x64400002), 50000, 53);
        let v = v4_view(&pkt).unwrap();
        // Zero out the UDP checksum ("no checksum") — a rewrite must not
        // resurrect it.
        pkt[v.ihl + 6] = 0;
        pkt[v.ihl + 7] = 0;
        rewrite_src(&mut pkt, &v, ip(0x6440001c));
        assert_eq!([pkt[v.ihl + 6], pkt[v.ihl + 7]], [0, 0]);

        // Never-zero property over a random walk: any nonzero stored
        // checksum stays nonzero after the patch.
        let mut lcg = Lcg(0xC0FFEE);
        let mut pkt = mk_udp(ip(0x64410005), ip(0x64400002), 50000, 53);
        for _ in 0..256 {
            let v = v4_view(&pkt).unwrap();
            let stored = u16::from_be_bytes([pkt[v.ihl + 6], pkt[v.ihl + 7]]);
            if stored != 0 {
                rewrite_src(&mut pkt, &v, ip(lcg.next()));
                let after = u16::from_be_bytes([pkt[v.ihl + 6], pkt[v.ihl + 7]]);
                assert_ne!(after, 0, "UDP checksum folded to on-the-wire zero");
            }
        }
    }

    #[test]
    fn flow_key_reverse_orientation_round_trips() {
        let me = ip(0x64410005);
        let peer = ip(0x64400002);
        // TCP + UDP: the reply (addresses and ports swapped) keys identically.
        for (out, back) in [
            (mk_tcp(me, peer, 50000, 22), mk_tcp(peer, me, 22, 50000)),
            (mk_udp(me, peer, 40000, 53), mk_udp(peer, me, 53, 40000)),
        ] {
            let vo = v4_view(&out).unwrap();
            let vb = v4_view(&back).unwrap();
            assert_eq!(
                egress_key(&out, &vo).unwrap(),
                ingress_key(&back, &vb).unwrap()
            );
        }
        // ICMP: request keys egress, reply keys ingress, same key.
        let req = mk_icmp(me, peer, 8, 1234);
        let rep = mk_icmp(peer, me, 0, 1234);
        let vr = v4_view(&req).unwrap();
        let vp = v4_view(&rep).unwrap();
        assert_eq!(
            egress_key(&req, &vr).unwrap(),
            ingress_key(&rep, &vp).unwrap()
        );
        // Orientation guards: a request never keys ingress (a peer's inbound
        // echo request must not match our outbound flow), a reply never keys
        // egress.
        assert!(ingress_key(&req, &vr).is_none());
        assert!(egress_key(&rep, &vp).is_none());
    }

    #[test]
    fn flow_map_ttl_expires_and_full_map_refuses() {
        let now = Instant::now();
        let mut m = FlowMap::default();
        let key = FlowKey {
            proto: PROTO_UDP,
            remote: ip(0x64400002),
            remote_port: 53,
            local_port: 40000,
        };
        assert!(m.note_egress(key, ip(0x64410005), ip(0x6440001c), now));
        // Wrong wire_src is inert.
        assert_eq!(m.restore_dst(&key, ip(0x64410009), now), None);
        // Fresh hit restores and refreshes.
        let later = now + Duration::from_secs(100);
        assert_eq!(
            m.restore_dst(&key, ip(0x6440001c), later),
            Some(ip(0x64410005))
        );
        // The refresh slid the window: +100 s more is still a hit…
        let later2 = later + Duration::from_secs(100);
        assert_eq!(
            m.restore_dst(&key, ip(0x6440001c), later2),
            Some(ip(0x64410005))
        );
        // …but past the UDP TTL it expires (and is removed).
        let gone = later2 + Duration::from_secs(121);
        assert_eq!(m.restore_dst(&key, ip(0x6440001c), gone), None);
        assert_eq!(m.len(), 0);

        // TCP gets the longer TTL.
        let tkey = FlowKey {
            proto: PROTO_TCP,
            ..key
        };
        assert!(m.note_egress(tkey, ip(0x64410005), ip(0x6440001c), now));
        assert_eq!(
            m.restore_dst(&tkey, ip(0x6440001c), now + Duration::from_secs(599)),
            Some(ip(0x64410005))
        );

        // Fill to cap with fresh entries: the next NEW key is refused, an
        // existing key still refreshes, and once everything expires the sweep
        // makes room.
        let mut m = FlowMap::default();
        for i in 0..FLOW_CAP as u32 {
            let k = FlowKey {
                proto: PROTO_UDP,
                remote: ip(0x64400002),
                remote_port: 53,
                local_port: (i & 0xFFFF) as u16,
            };
            let k = FlowKey {
                remote: ip(0x64400002 + (i >> 16)),
                ..k
            };
            assert!(m.note_egress(k, ip(0x64410005), ip(0x6440001c), now));
        }
        let fresh = FlowKey {
            proto: PROTO_UDP,
            remote: ip(0x0A000001),
            remote_port: 1,
            local_port: 1,
        };
        assert!(!m.note_egress(fresh, ip(0x64410005), ip(0x6440001c), now));
        let existing = FlowKey {
            proto: PROTO_UDP,
            remote: ip(0x64400002),
            remote_port: 53,
            local_port: 7,
        };
        assert!(m.note_egress(existing, ip(0x64410005), ip(0x6440001c), now));
        let expired_now = now + Duration::from_secs(121);
        assert!(m.note_egress(fresh, ip(0x64410005), ip(0x6440001c), expired_now));

        // purge_addr drops by either side.
        let mut m = FlowMap::default();
        assert!(m.note_egress(key, ip(0x64410005), ip(0x6440001c), now));
        m.purge_addr(ip(0x64410005));
        assert_eq!(m.len(), 0);
        assert!(m.note_egress(key, ip(0x64410005), ip(0x6440001c), now));
        m.purge_addr(ip(0x6440001c));
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn v4_view_flags_fragments_and_rejects_junk() {
        let mut pkt = mk_udp(ip(0x64410005), ip(0x64400002), 40000, 53);
        let v = v4_view(&pkt).unwrap();
        assert!(!v.fragment);
        assert!(v.first_fragment);

        // MF set, offset 0: fragment AND first_fragment.
        pkt[6] = 0x20;
        let v = v4_view(&pkt).unwrap();
        assert!(v.fragment && v.first_fragment);

        // Nonzero offset: fragment, not first.
        pkt[6] = 0x00;
        pkt[7] = 0x05;
        let v = v4_view(&pkt).unwrap();
        assert!(v.fragment && !v.first_fragment);

        // Junk: v6 nibble, truncated, IHL < 20.
        assert!(v4_view(&[0x60; 40]).is_none());
        assert!(v4_view(&[0x45; 12]).is_none());
        let mut bad_ihl = mk_udp(ip(1), ip(2), 1, 2);
        bad_ihl[0] = 0x44;
        assert!(v4_view(&bad_ihl).is_none());
    }
}

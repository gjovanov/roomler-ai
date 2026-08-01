//! The ONE worker-pick implementation (overlay-consolidation invariant I6).
//!
//! Three subsystems must deterministically map a key onto one coturn worker so
//! that independently-acting ends CO-LOCATE on the same worker:
//!
//! 1. **overlay client** (`tunnel-core` `relay_link::pick_worker`) — both
//!    peers of a relay pair hash the server-issued `pair_key` over the
//!    DNS-resolved worker set;
//! 2. **overlay server** (`api` `ws/overlay.rs` grant pinning, `&pin=`) — the
//!    broker computes the same pick authoritatively over its cached resolve
//!    of the identical DNS name;
//! 3. **remote-control TURN creds** (`turn_creds::issue_for_session`) —
//!    controller and agent creds put the same session-picked worker's URLs
//!    first, so both ICE stacks converge on it.
//!
//! Co-location is an *invariant*, not an optimisation: cross-worker
//! relay↔relay traffic straddles the workers' public interfaces, and the
//! dual-public-IP worker's SNAT asymmetry drops it (the both-allocate REKEY
//! failure — docs/overlay-nat-traversal.md, "Worker co-location"); even where
//! it survives, it adds a public-internet hop. Ends therefore MUST agree,
//! across crates and across releases — so the hash lives HERE once, and every
//! consumer carries a golden-vector test pinning byte-identical agreement
//! (grep for `worker-pick golden vector`).
//!
//! FNV-1a is not security-sensitive here (load spreading / rendezvous only);
//! it just has to be stable across processes and nodes, which the stdlib
//! `DefaultHasher` (per-process seeded) is not.

use std::net::IpAddr;

/// Stable 64-bit FNV-1a (offset basis `0xcbf29ce484222325`, prime
/// `0x100000001b3`).
pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Deterministic slot pick: `FNV-1a(key) % n`. `None` when `n == 0`.
///
/// For indexing a list that is FIXED and identically ordered on every caller
/// (e.g. the configured per-worker TURN URL groups — both issuances happen in
/// the same Hub process). When callers assemble the candidate list
/// independently, use [`pick_worker_fnv1a`], which canonicalises it first.
pub fn pick_index_fnv1a(key: &str, n: usize) -> Option<usize> {
    (n > 0).then(|| (fnv1a_64(key.as_bytes()) % n as u64) as usize)
}

/// Canonical worker-IP pick: retain IPv4 → sort → dedup → index by
/// [`pick_index_fnv1a`]. The canonicalisation makes the pick independent of
/// DNS answer order, so ends that resolved the worker set separately still
/// agree. `None` when no IPv4 candidate remains — callers degrade to
/// unpinned (round-robin) rather than fail.
pub fn pick_worker_fnv1a(key: &str, mut ips: Vec<IpAddr>) -> Option<IpAddr> {
    ips.retain(IpAddr::is_ipv4);
    ips.sort();
    ips.dedup();
    let idx = pick_index_fnv1a(key, ips.len())?;
    Some(ips[idx])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    /// Published FNV-1a 64 reference vectors — pins offset basis + prime to
    /// the standard algorithm, not merely to "whatever we shipped".
    #[test]
    fn fnv1a_reference_vectors() {
        assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a_64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a_64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    /// THE worker-pick golden vector. Each consumer (relay_link, ws/overlay
    /// grant pinning, turn_creds) re-asserts these exact literals through its
    /// own call path — if any site drifts from this module, a golden test
    /// fails somewhere.
    #[test]
    fn worker_pick_golden_vector() {
        // pair_key shape: sorted ObjectId-hex pair (api `pair_key()`).
        let key = "507f1f77bcf86cd799439011:507f1f77bcf86cd799439012";
        assert_eq!(fnv1a_64(key.as_bytes()), 0xad37_bde0_cdd9_5470);
        let ips: Vec<IpAddr> = vec![
            "203.0.113.9".parse().unwrap(),
            "198.51.100.4".parse().unwrap(),
            "203.0.113.7".parse().unwrap(),
        ];
        // sorted: [198.51.100.4, 203.0.113.7, 203.0.113.9]; 0x…5470 % 3 = 2.
        assert_eq!(
            pick_worker_fnv1a(key, ips),
            Some("203.0.113.9".parse().unwrap())
        );

        // session_key shape: session ObjectId hex (turn_creds).
        assert_eq!(pick_index_fnv1a("6a54bf440b4fd609a7356f97", 3), Some(0));
    }

    #[test]
    fn pick_is_order_independent_and_dedups() {
        let key = "507f1f77bcf86cd799439011:507f1f77bcf86cd799439012";
        let a: IpAddr = "198.51.100.4".parse().unwrap();
        let b: IpAddr = "203.0.113.7".parse().unwrap();
        let c: IpAddr = "203.0.113.9".parse().unwrap();
        let p = pick_worker_fnv1a(key, vec![a, b, c]).unwrap();
        assert_eq!(p, pick_worker_fnv1a(key, vec![c, a, b]).unwrap());
        assert_eq!(p, pick_worker_fnv1a(key, vec![b, c, a, b]).unwrap());
    }

    #[test]
    fn pick_filters_v6_and_handles_empty() {
        let key = "any";
        let v4: IpAddr = "203.0.113.7".parse().unwrap();
        let v6: IpAddr = IpAddr::V6(Ipv6Addr::LOCALHOST);
        assert_eq!(pick_worker_fnv1a(key, vec![v6, v4]), Some(v4));
        assert_eq!(pick_worker_fnv1a(key, vec![v6]), None);
        assert_eq!(pick_worker_fnv1a(key, vec![]), None);
        assert_eq!(pick_index_fnv1a(key, 0), None);
    }

    #[test]
    fn distinct_keys_spread_across_slots() {
        let ips: Vec<IpAddr> = (1..=4u8)
            .map(|o| IpAddr::V4(Ipv4Addr::new(203, 0, 113, o)))
            .collect();
        let mut seen = std::collections::HashSet::new();
        for i in 0..64 {
            seen.insert(pick_worker_fnv1a(&format!("session-{i}"), ips.clone()).unwrap());
        }
        assert!(seen.len() > 1, "hash must actually spread load");
    }
}

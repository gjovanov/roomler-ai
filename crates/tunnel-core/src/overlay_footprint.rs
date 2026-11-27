// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #1882 — the address space this process's overlay runs on, for code
//! OUTSIDE the overlay that has to tell whether an address is reached through
//! the mesh. Remote desktop is that code today: with a device's `rc_overlay`
//! off, its ICE keeps every overlay address out of a session.
//!
//! **v4** is learned from each runtime's netmap — every block of its org
//! (FR-47 P5e: a peer may sit in another block than this node), for every
//! org the device is in. #3359: it is recorded when the netmap names the
//! blocks, BEFORE the runtime calls its TUN factory, so the footprint is
//! known before the adapter holds an overlay address; a later full netmap
//! adds a block the org has grown. It only ever GROWS. A block this process
//! has routed through the mesh stays "overlay" until the process exits, so
//! there is no window (a WS reconnect, a runtime restart) in which an
//! overlay address briefly reads as foreign. The cost is a block an org gave
//! up, which nothing outside the mesh can be using anyway.
//!
//! #3359 — a reader judges an address by the footprint AS IT STANDS
//! ([`is_overlay_addr_now`]), never only by a copy taken earlier, and a
//! reader that hands the footprint on (the daemon to a macOS GUI worker)
//! hears each growth at once ([`grown`]).
//!
//! **v6** needs no registry: every overlay v6 is derived into Roomler's fixed
//! ULA `/96` (`fd72:6f6f:6d6c::/96`, `overlay::router::OVERLAY_ULA_V6_CIDR`).
//!
//! Deliberately not the whole `100.64.0.0/10`. That is real CGNAT space: an
//! LTE modem, a cloud VPC's secondary range or another mesh (Tailscale) hands
//! out addresses in it that are not ours, and a CIDR rule would strip them.
//!
//! Outside the `overlay` feature on purpose: the overlay WRITES this, but a
//! build without one (the signalling-only agent) still asks, and then simply
//! has no v4 blocks.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{LazyLock, Mutex};

/// ⚠️ A std mutex, held only inside the synchronous functions below, never
/// across an `.await`: ICE's IP filter reads it from inside the agent's
/// gather.
static V4_NETS: Mutex<Vec<(Ipv4Addr, u8)>> = Mutex::new(Vec::new());

/// #3359 — bumped each time [`V4_NETS`] gains a block, after the registry's
/// lock is released. Lives for the process, so a receiver never sees it
/// closed.
static GROWN: LazyLock<tokio::sync::watch::Sender<u64>> =
    LazyLock::new(|| tokio::sync::watch::channel(0).0);

/// The fixed `/96` every overlay v6 is derived into. A copy of
/// `overlay::router`'s pinned hextets (that module is feature-gated); the
/// `derived_v6_matches_the_router` test keeps the two equal.
const OVERLAY_ULA_96: [u16; 6] = [0xfd72, 0x6f6f, 0x6d6c, 0, 0, 0];

fn in_overlay_ula(v6: Ipv6Addr) -> bool {
    v6.segments()[..6] == OVERLAY_ULA_96
}

/// `"100.65.4.0/22"` → `(100.65.4.0, 22)`, masked to its network address.
/// `None` for anything that is not a v4 CIDR.
fn parse_v4_cidr(cidr: &str) -> Option<(Ipv4Addr, u8)> {
    let (addr, plen) = cidr.trim().split_once('/')?;
    let addr: Ipv4Addr = addr.trim().parse().ok()?;
    let plen: u8 = plen.trim().parse().ok()?;
    if plen > 32 {
        return None;
    }
    Some((Ipv4Addr::from(u32::from(addr) & prefix_mask(plen)), plen))
}

fn v4_in(net: (Ipv4Addr, u8), ip: Ipv4Addr) -> bool {
    let mask = prefix_mask(net.1);
    u32::from(ip) & mask == u32::from(net.0) & mask
}

/// Record the v4 blocks an overlay runtime routes (its org's `cidrs`, or the
/// single `cidr` from an older server). Idempotent; unparseable entries are
/// skipped.
pub fn note_v4_nets(cidrs: &[String]) {
    let parsed: Vec<_> = cidrs.iter().filter_map(|c| parse_v4_cidr(c)).collect();
    note_v4_blocks(&parsed);
}

/// [`note_v4_nets`] for blocks that are already parsed: what a process that
/// runs NO overlay is handed by one that does. A supervised macOS worker
/// serves its own enrollment's sessions from the user's GUI session, while
/// the overlay lives in the root daemon; without this its footprint is empty
/// and its own sessions would read the mesh as foreign (#1882). Each block is
/// masked to its network address; a prefix over 32 is dropped.
pub fn note_v4_blocks(blocks: &[(Ipv4Addr, u8)]) {
    if blocks.is_empty() {
        return;
    }
    let mut grew = false;
    if let Ok(mut g) = V4_NETS.lock() {
        for &(addr, plen) in blocks {
            if plen > 32 {
                continue;
            }
            let n = (Ipv4Addr::from(u32::from(addr) & prefix_mask(plen)), plen);
            if !g.contains(&n) {
                g.push(n);
                grew = true;
            }
        }
    }
    // #3359 — outside the lock: a woken reader takes it to read the set.
    if grew {
        GROWN.send_modify(|n| *n = n.wrapping_add(1));
    }
}

/// #3359 — a receiver that wakes each time the footprint gains a block (a
/// repeat of a known one wakes nobody). It starts with every growth so far
/// already seen, so subscribe BEFORE reading [`v4_nets`] for the first time,
/// and nothing recorded in between is missed.
pub fn grown() -> tokio::sync::watch::Receiver<u64> {
    GROWN.subscribe()
}

fn prefix_mask(plen: u8) -> u32 {
    if plen == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(plen.min(32)))
    }
}

/// Every v4 block noted so far in this process. Empty in a process that runs
/// no overlay, such as a macOS GUI worker: that one is handed the daemon's
/// snapshot instead.
pub fn v4_nets() -> Vec<(Ipv4Addr, u8)> {
    V4_NETS.lock().map(|g| g.clone()).unwrap_or_default()
}

/// Is `ip` an overlay address: inside one of `v4_nets`, or a derived overlay
/// v6? An IPv4-mapped v6 is judged as the v4 it carries.
pub fn is_overlay_addr(v4_nets: &[(Ipv4Addr, u8)], ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4_nets.iter().any(|n| v4_in(*n, v4)),
        IpAddr::V6(v6) => in_overlay_ula(v6),
    }
}

/// #3359 — [`is_overlay_addr`] against the footprint as it stands at this
/// call: every block recorded so far, with no copy. What a filter asks when
/// it judges an address, so a block recorded after the filter was made
/// still counts.
pub fn is_overlay_addr_now(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => V4_NETS
            .lock()
            .map(|g| g.iter().any(|n| v4_in(*n, v4)))
            .unwrap_or(false),
        IpAddr::V6(v6) => in_overlay_ula(v6),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn cidrs_parse_masked_and_garbage_is_skipped() {
        assert_eq!(
            parse_v4_cidr("100.65.4.17/22"),
            Some((Ipv4Addr::new(100, 65, 4, 0), 22))
        );
        assert_eq!(
            parse_v4_cidr(" 100.64.0.0/10 "),
            Some((Ipv4Addr::new(100, 64, 0, 0), 10))
        );
        assert_eq!(parse_v4_cidr("100.65.4.0/33"), None);
        assert_eq!(parse_v4_cidr("fd72:6f6f:6d6c::/96"), None);
        assert_eq!(parse_v4_cidr("100.65.4.0"), None);
    }

    /// Only OUR blocks are overlay — not the rest of the CGNAT `/10`, which an
    /// LTE modem or another mesh may be using — and every derived v6 is.
    #[test]
    fn only_our_blocks_and_the_derived_ula_are_overlay() {
        let nets = [
            parse_v4_cidr("100.65.4.0/22").unwrap(),
            parse_v4_cidr("100.65.8.0/22").unwrap(),
        ];
        assert!(is_overlay_addr(&nets, ip("100.65.4.34")));
        assert!(is_overlay_addr(&nets, ip("100.65.11.255")));
        assert!(!is_overlay_addr(&nets, ip("100.65.12.1")));
        assert!(!is_overlay_addr(&nets, ip("100.101.102.103")), "a tailnet");
        assert!(!is_overlay_addr(&nets, ip("192.168.1.20")));
        assert!(
            is_overlay_addr(&nets, ip("::ffff:100.65.4.34")),
            "mapped v4"
        );
        assert!(is_overlay_addr(&nets, ip("fd72:6f6f:6d6c::6441:422")));
        assert!(
            !is_overlay_addr(&nets, ip("fd00::1")),
            "a home router's ULA"
        );
        assert!(!is_overlay_addr(&nets, ip("2001:db8::1")));
        // With nothing noted, no v4 is overlay; the derived v6 still is.
        assert!(!is_overlay_addr(&[], ip("100.65.4.34")));
        assert!(is_overlay_addr(&[], ip("fd72:6f6f:6d6c::6441:422")));
    }

    /// The local copy of the ULA prefix must be the router's: a node's own
    /// derived v6, and the router's inverse, agree with it.
    #[cfg(feature = "overlay")]
    #[test]
    fn derived_v6_matches_the_router() {
        use crate::overlay::router::{derive_overlay_v6, embedded_v4_of_overlay_v6};
        for v4 in [Ipv4Addr::new(100, 65, 4, 34), Ipv4Addr::new(100, 64, 0, 7)] {
            let v6 = derive_overlay_v6(v4);
            assert!(in_overlay_ula(v6), "{v6}");
            assert_eq!(embedded_v4_of_overlay_v6(v6), Some(v4));
        }
        let foreign: Ipv6Addr = "fd72:6f6f:6d6d::1".parse().unwrap();
        assert_eq!(
            in_overlay_ula(foreign),
            embedded_v4_of_overlay_v6(foreign).is_some()
        );
    }

    #[test]
    fn noting_is_idempotent_and_grows() {
        // Process-global: assert only on blocks this test owns.
        let a = "100.66.0.0/22".to_string();
        let b = "100.66.4.0/22".to_string();
        note_v4_nets(std::slice::from_ref(&a));
        note_v4_nets(&[a.clone(), "junk".into()]);
        note_v4_nets(&[b]);
        let nets = v4_nets();
        let ours = |n: &(Ipv4Addr, u8)| n.0.octets()[..2] == [100, 66];
        assert_eq!(nets.iter().filter(|n| ours(n)).count(), 2);
        assert!(is_overlay_addr(&nets, ip("100.66.5.1")));
    }

    /// #3359 — a block counts the moment it is recorded: the footprint as it
    /// stands answers for it with no copy taken, and a reader subscribed
    /// earlier is woken. (Positive cells only: the runtime's tests record
    /// the whole `/10` in this process, through their pre-P5d netmap.)
    #[test]
    fn a_recorded_block_counts_at_once_and_wakes_a_subscriber() {
        // Process-global: assert only on a block this test owns.
        let mut rx = grown();
        note_v4_nets(&["100.69.28.0/22".to_string()]);
        assert!(rx.has_changed().unwrap(), "the growth woke the subscriber");
        rx.borrow_and_update();
        assert!(is_overlay_addr_now(ip("100.69.28.9")));
        assert!(is_overlay_addr_now(ip("::ffff:100.69.31.255")), "mapped v4");
        assert!(v4_nets().contains(&(Ipv4Addr::new(100, 69, 28, 0), 22)));
        assert!(!is_overlay_addr_now(ip("192.0.2.1")));
        assert!(is_overlay_addr_now(ip("fd72:6f6f:6d6c::6445:1c09")));
        assert!(!is_overlay_addr_now(ip("fd00::1")), "a home router's ULA");
    }

    /// What a worker is handed: already parsed, but not necessarily masked.
    /// A host address with its prefix is stored as the block, a repeat is not
    /// stored twice, and an impossible prefix is dropped.
    #[test]
    fn handed_blocks_are_masked_deduplicated_and_sane() {
        // Process-global: assert only on blocks this test owns.
        note_v4_blocks(&[
            (Ipv4Addr::new(100, 67, 4, 34), 22),
            (Ipv4Addr::new(100, 67, 4, 0), 22),
            (Ipv4Addr::new(100, 67, 9, 9), 40),
        ]);
        let nets = v4_nets();
        let ours: Vec<_> = nets
            .iter()
            .filter(|n| n.0.octets()[..2] == [100, 67])
            .collect();
        assert_eq!(ours, [&(Ipv4Addr::new(100, 67, 4, 0), 22)]);
        assert!(is_overlay_addr(&nets, ip("100.67.7.255")));
    }
}

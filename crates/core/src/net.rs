// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Address rules shared across pillars (FR-69 P7a): the push route's SSRF
//! check (core) and the peer-relay static-endpoint check (network) agree on
//! what "a routable public address" means, and neither may name the other's
//! file — so the predicate lives here.

use std::net::IpAddr;

/// Conservative "is this a routable public address" test. Deliberately hand
/// rolled: `IpAddr::is_global` is still unstable.
pub fn is_global_unicast(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()   // 169.254/16 — cloud metadata
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.octets()[0] == 0
                || v4.octets()[0] == 127
                // 100.64/10 CGNAT — also the overlay mesh range
                || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
                // 192.0.0/24 IETF protocol assignments
                || (v4.octets()[0] == 192 && v4.octets()[1] == 0 && v4.octets()[2] == 0)
                // 198.18/15 benchmarking
                || (v4.octets()[0] == 198 && (18..20).contains(&v4.octets()[1]))
                // 240/4 reserved
                || v4.octets()[0] >= 240)
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // fc00::/7 unique-local
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // fe80::/10 link-local
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // v4-mapped/compatible: re-check the embedded v4
                || v6
                    .to_ipv4_mapped()
                    .map(|v4| !is_global_unicast(&IpAddr::V4(v4)))
                    .unwrap_or(false)
                || v6
                    .to_ipv4()
                    .map(|v4| !is_global_unicast(&IpAddr::V4(v4)))
                    .unwrap_or(false))
        }
    }
}

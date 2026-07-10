//! Auto-detection of local IPv4 subnets for the tunnel mesh subnet-router.
//!
//! The agent advertises the CIDRs it can route (`rc:agent.hello`
//! `advertised_routes`) so an admin can one-click-approve them (Admin → Agents
//! → Subnet routes) into the agent's effective `routes`. Rather than force the
//! operator to hand-list every subnet, this enumerates the host's own network
//! interfaces and offers each directly-connected IPv4 subnet as a suggestion —
//! unioned with any explicit `advertise_routes` config.
//!
//! Everything here is UNTRUSTED until an admin approves it, so the filter is
//! deliberately lenient: it only drops addresses that are never a routable LAN
//! (loopback, link-local, CGNAT, broadcast/multicast/unspecified) and
//! point-to-point (/31, /32) assignments. Virtual adapters (WSL, Docker,
//! Hyper-V) may still surface as suggestions — harmless noise the admin ignores.

use std::net::Ipv4Addr;

use crate::config::AgentConfig;

/// The full set of subnet CIDRs this agent advertises on hello: explicit
/// `advertise_routes` config (validated + canonicalized) unioned with
/// auto-detected local IPv4 subnets (unless `advertise_local_subnets` is off).
pub fn local_advertised_routes(cfg: &AgentConfig) -> Vec<String> {
    advertised_routes(&cfg.advertise_routes, cfg.advertise_local_subnets)
}

/// Core of [`local_advertised_routes`], split out so it's testable without a
/// full `AgentConfig`: canonicalize + dedup the explicit CIDRs (skipping
/// invalid), then — if `include_local` — union the auto-detected local subnets.
/// Order-preserving.
fn advertised_routes(explicit: &[String], include_local: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in explicit {
        match r.trim().parse::<ipnet::IpNet>() {
            Ok(net) => {
                let c = net.trunc().to_string();
                if !out.contains(&c) {
                    out.push(c);
                }
            }
            Err(_) => tracing::warn!(route = %r, "advertise_routes: skipping invalid CIDR"),
        }
    }
    if include_local {
        for c in detect_local_subnets() {
            if !out.contains(&c) {
                out.push(c);
            }
        }
    }
    out
}

/// Enumerate the host's network interfaces and return each directly-connected
/// IPv4 subnet as a canonical CIDR (network address / prefix). IPv6 is skipped
/// for auto-detect (privacy / temporary addresses are noisy — advertise IPv6
/// explicitly via `advertise_routes`). Returns empty on enumeration failure.
pub fn detect_local_subnets() -> Vec<String> {
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for iface in ifaces {
        let if_addrs::IfAddr::V4(v4) = iface.addr else {
            continue;
        };
        if !is_advertisable_v4(&v4.ip) {
            continue;
        }
        let Ok(net) = ipnet::Ipv4Net::with_netmask(v4.ip, v4.netmask) else {
            continue;
        };
        // /31 and /32 are point-to-point / single-host (VPN, RFC 3021) links,
        // not a routable LAN behind this host — skip.
        if net.prefix_len() >= 31 {
            continue;
        }
        let cidr = net.trunc().to_string();
        if !out.contains(&cidr) {
            out.push(cidr);
        }
    }
    out
}

/// True when `ip` could be a routable LAN address worth advertising. Excludes
/// loopback, link-local (169.254/16), CGNAT (100.64/10 — the overlay /
/// Tailscale space), and unspecified / broadcast / multicast.
fn is_advertisable_v4(ip: &Ipv4Addr) -> bool {
    let o = ip.octets();
    let is_cgnat = o[0] == 100 && (o[1] & 0xc0) == 0x40;
    !(ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || is_cgnat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertisable_v4_filters() {
        // Routable private + global → advertisable.
        assert!(is_advertisable_v4(&Ipv4Addr::new(192, 168, 1, 10)));
        assert!(is_advertisable_v4(&Ipv4Addr::new(10, 66, 24, 53)));
        // WSL-style — still offered (admin-gated, harmless noise).
        assert!(is_advertisable_v4(&Ipv4Addr::new(172, 19, 233, 180)));
        assert!(is_advertisable_v4(&Ipv4Addr::new(8, 8, 8, 8)));
        // Never a LAN → dropped.
        assert!(!is_advertisable_v4(&Ipv4Addr::new(127, 0, 0, 1)));
        assert!(!is_advertisable_v4(&Ipv4Addr::new(169, 254, 1, 1)));
        assert!(!is_advertisable_v4(&Ipv4Addr::new(0, 0, 0, 0)));
        assert!(!is_advertisable_v4(&Ipv4Addr::new(255, 255, 255, 255)));
        assert!(!is_advertisable_v4(&Ipv4Addr::new(224, 0, 0, 1)));
        // CGNAT 100.64/10 (overlay space).
        assert!(!is_advertisable_v4(&Ipv4Addr::new(100, 64, 0, 1)));
        assert!(!is_advertisable_v4(&Ipv4Addr::new(100, 127, 255, 254)));
        // 100.0.0.0/8 outside the CGNAT sub-range is fine.
        assert!(is_advertisable_v4(&Ipv4Addr::new(100, 0, 0, 1)));
    }

    #[test]
    fn explicit_routes_canonicalize_dedup_and_skip_invalid() {
        // `include_local = false` isolates the explicit path from the host's
        // real interfaces so the assertion is deterministic.
        let out = advertised_routes(
            &[
                "10.0.0.5/24".into(), // host bits set → masked to network
                "bad-cidr".into(),    // invalid → skipped
                "10.0.0.0/24".into(), // dup once canonicalized
            ],
            false,
        );
        assert_eq!(out, vec!["10.0.0.0/24".to_string()]);
    }

    #[test]
    #[ignore = "machine-dependent; run with `--ignored --nocapture` to eyeball"]
    fn eyeball_detect_local_subnets() {
        println!("detect_local_subnets() -> {:?}", detect_local_subnets());
    }
}

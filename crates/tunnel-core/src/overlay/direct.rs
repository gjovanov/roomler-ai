//! Direct (LAN) carrier discovery for the overlay (rc.131).
//!
//! The overlay was relay-only: every peer connection rode a coturn TURN
//! allocation, even two machines on the same Wi-Fi LAN. That made it fragile
//! (it dies whenever a node can't reach coturn — UDP-blocked / TLS-inspected
//! corporate nets, carrier-CGNAT cellular) and added a relay hop's latency to
//! same-LAN peers. This module adds the **direct LAN path** (Tailscale's
//! direct-first model): a node advertises its LAN endpoint, and two peers on
//! the **same /24** build a direct UDP [`Carrier`](super::wg::Carrier) and skip
//! the relay entirely.
//!
//! v1 scope: **same-subnet only** (reliable L2 reachability — no NAT
//! hole-punch, no handshake-timeout fallback). Peers NOT on a shared subnet
//! still use the relay exactly as before. srflx hole-punch + an AP-isolation
//! relay-fallback are follow-ups (rc.132). See `docs/overlay-wfp.md` siblings.

use std::net::{Ipv4Addr, SocketAddr};

/// `ROOMLER_AGENT_OVERLAY_DIRECT` — default **ON**. Set `0`/`false`/`no`/`off`
/// to disable the direct LAN path and force pure relay (the pre-rc.131
/// behaviour) if a field host misbehaves. Matches the agent's truthy
/// convention (and the WFP gate's).
pub fn direct_enabled() -> bool {
    match std::env::var("ROOMLER_AGENT_OVERLAY_DIRECT") {
        Ok(v) => {
            let t = v.trim();
            !(t.eq_ignore_ascii_case("0")
                || t.eq_ignore_ascii_case("false")
                || t.eq_ignore_ascii_case("no")
                || t.eq_ignore_ascii_case("off"))
        }
        Err(_) => true,
    }
}

/// Discover this node's primary LAN IPv4 — the source address the OS would
/// use to reach the internet (the default-route interface). Uses the
/// "connect a UDP socket and read its local address" trick: `connect` on a
/// UDP socket sends nothing, it just fixes the route, so `local_addr()` then
/// reveals the chosen source IP. No interface-enumeration crate, no packets.
///
/// Returns `None` if there's no route (offline / CI sandbox) or the source is
/// not a usable LAN address (loopback / link-local / unspecified). The overlay
/// CGNAT range `100.64.0.0/10` is also rejected so a carrier-CGNAT cellular
/// interface (which collides with the overlay) is never advertised as a LAN
/// endpoint.
pub async fn primary_lan_ip() -> Option<Ipv4Addr> {
    // Two well-known public IPs; the dst is never contacted (connect only sets
    // the route). Try a second if the first has no route.
    for probe in ["1.1.1.1:80", "8.8.8.8:80"] {
        let Ok(sock) = tokio::net::UdpSocket::bind("0.0.0.0:0").await else {
            continue;
        };
        if sock.connect(probe).await.is_err() {
            continue;
        }
        if let Ok(SocketAddr::V4(local)) = sock.local_addr() {
            let ip = *local.ip();
            if is_usable_lan_ipv4(ip) {
                return Some(ip);
            }
        }
    }
    None
}

/// True for an IPv4 that can serve as a same-LAN endpoint: not loopback, not
/// link-local (169.254), not unspecified/broadcast, and not in the overlay
/// CGNAT range `100.64.0.0/10` (which collides with both the overlay itself
/// and some cellular carriers).
pub fn is_usable_lan_ipv4(ip: Ipv4Addr) -> bool {
    !ip.is_loopback()
        && !ip.is_link_local()
        && !ip.is_unspecified()
        && !ip.is_broadcast()
        && !is_cgnat(ip)
}

/// `100.64.0.0/10` (RFC 6598 carrier-grade NAT) — also the overlay's own
/// address range.
fn is_cgnat(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (64..=127).contains(&o[1])
}

/// Same-/24 test: two IPv4s share the top 24 bits. A strong, conservative
/// signal of same-L2-segment reachability for home/office LANs (good enough
/// for v1; a netmask-aware check is a refinement).
pub fn same_subnet_24(a: Ipv4Addr, b: Ipv4Addr) -> bool {
    let (a, b) = (a.octets(), b.octets());
    a[0] == b[0] && a[1] == b[1] && a[2] == b[2]
}

/// From a peer's advertised `endpoints` (host/srflx/relay strings), pick the
/// first that is a directly-dialable host endpoint **on our LAN** — i.e. an
/// `IP:port` whose IP is in our /24. `None` if the peer advertised no
/// same-subnet endpoint (→ caller falls back to the relay).
pub fn pick_same_subnet_endpoint(my_ip: Ipv4Addr, endpoints: &[String]) -> Option<SocketAddr> {
    for ep in endpoints {
        // Tolerate scheme-ish prefixes defensively; we only emit bare IP:port.
        let raw = ep.trim();
        if let Ok(SocketAddr::V4(sa)) = raw.parse::<SocketAddr>()
            && same_subnet_24(my_ip, *sa.ip())
            && is_usable_lan_ipv4(*sa.ip())
        {
            return Some(SocketAddr::V4(sa));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgnat_and_lan_classification() {
        assert!(is_usable_lan_ipv4("192.168.68.103".parse().unwrap()));
        assert!(is_usable_lan_ipv4("10.16.6.34".parse().unwrap()));
        assert!(!is_usable_lan_ipv4("127.0.0.1".parse().unwrap()));
        assert!(!is_usable_lan_ipv4("169.254.1.2".parse().unwrap()));
        assert!(!is_usable_lan_ipv4("0.0.0.0".parse().unwrap()));
        // CGNAT / overlay range rejected (the cellular-carrier collision).
        assert!(!is_usable_lan_ipv4("100.64.0.1".parse().unwrap()));
        assert!(!is_usable_lan_ipv4("100.127.255.1".parse().unwrap()));
        assert!(is_usable_lan_ipv4("100.128.0.1".parse().unwrap())); // just outside /10
    }

    #[test]
    fn same_subnet_24_matches_lan_pairs() {
        let a: Ipv4Addr = "192.168.68.103".parse().unwrap();
        let b: Ipv4Addr = "192.168.68.110".parse().unwrap();
        let c: Ipv4Addr = "192.168.69.110".parse().unwrap();
        assert!(same_subnet_24(a, b), "PC50045 + NEO16 are same /24");
        assert!(!same_subnet_24(a, c), "different /24");
    }

    #[test]
    fn picks_same_subnet_host_endpoint_else_none() {
        let me: Ipv4Addr = "192.168.68.103".parse().unwrap();
        // Mixed endpoint list: a far srflx, the relay, and the LAN host.
        let eps = vec![
            "37.63.112.129:49358".to_string(),  // srflx (different /24) — skip
            "94.130.141.74:3478".to_string(),   // relay — skip
            "192.168.68.110:51820".to_string(), // same /24 — pick this
        ];
        let got = pick_same_subnet_endpoint(me, &eps).unwrap();
        assert_eq!(got, "192.168.68.110:51820".parse::<SocketAddr>().unwrap());

        // No same-subnet endpoint → None (caller uses relay).
        let only_far = vec!["37.63.112.129:49358".to_string()];
        assert!(pick_same_subnet_endpoint(me, &only_far).is_none());

        // A same-subnet but CGNAT endpoint is rejected.
        let cgnat = vec!["100.64.0.110:51820".to_string()];
        assert!(pick_same_subnet_endpoint("100.64.0.103".parse().unwrap(), &cgnat).is_none());
    }
}

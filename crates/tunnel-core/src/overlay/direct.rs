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
//! Scope: **same-subnet only** (reliable L2 reachability — no NAT hole-punch,
//! no handshake-timeout fallback). Peers NOT on a shared subnet still use the
//! relay exactly as before. rc.131 advertised one interface (a connect-trick);
//! **rc.132 enumerates ALL interfaces** (a multi-homed host advertises every
//! LAN IP — field host PC50045 routes the internet via corporate Ethernet but
//! its peer is on the Wi-Fi). srflx hole-punch + an AP-isolation relay-fallback
//! are later follow-ups. See `docs/overlay-wfp.md` siblings.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{UdpSocket, lookup_host};

/// `ROOMLER_NODE_OVERLAY_DIRECT` (legacy `ROOMLER_AGENT_OVERLAY_DIRECT` still
/// honoured — see [`crate::env::node_env`]) — default **ON**. Set
/// `0`/`false`/`no`/`off` to disable the direct LAN path and force pure relay
/// (the pre-rc.131 behaviour) if a field host misbehaves. Matches the node's
/// truthy convention (and the WFP gate's).
pub fn direct_enabled() -> bool {
    match crate::env::node_env("OVERLAY_DIRECT") {
        Some(v) => {
            let t = v.trim();
            !(t.eq_ignore_ascii_case("0")
                || t.eq_ignore_ascii_case("false")
                || t.eq_ignore_ascii_case("no")
                || t.eq_ignore_ascii_case("off"))
        }
        None => true,
    }
}

/// Enumerate this node's usable LAN IPv4 addresses across **all** interfaces,
/// so a multi-homed host advertises every LAN endpoint and a peer matches
/// whichever is on its subnet.
///
/// rc.132 — replaces the rc.131 connect-trick (default-route IP only), which
/// picked the WRONG interface on a multi-homed host: field host PC50045 routes
/// the internet via its corporate Ethernet (`172.30.x`) but its overlay peer
/// (NEO16) is on the Wi-Fi (`192.168.68.x`), so the single default-route IP it
/// advertised was unreachable by the peer → no same-subnet match → fell back
/// to the (failing) relay. Enumerating all interfaces advertises both, so the
/// peer finds the `192.168.68.x` one.
///
/// Excludes loopback / link-local / CGNAT (`100.64.0.0/10` — the overlay's own
/// range + some cellular carriers). Order is `get_if_addrs`' (stable enough);
/// dups removed. Empty if enumeration fails (→ relay only, as before).
pub fn gather_lan_ips() -> Vec<Ipv4Addr> {
    gather_lan_interfaces()
        .into_iter()
        .map(|(ip, _)| ip)
        .collect()
}

/// Like [`gather_lan_ips`] but also returns each interface's OS index (for
/// `IP_UNICAST_IF` egress pinning — rc.144). The index is `None` when
/// `if-addrs` can't supply one (then egress can't be pinned — the socket falls
/// back to rc.143 source-IP binding only). Deduped by IP.
pub fn gather_lan_interfaces() -> Vec<(Ipv4Addr, Option<u32>)> {
    let mut out: Vec<(Ipv4Addr, Option<u32>)> = Vec::new();
    if let Ok(addrs) = if_addrs::get_if_addrs() {
        for a in addrs {
            if let std::net::IpAddr::V4(ip) = a.ip()
                && is_usable_lan_ipv4(ip)
                && !out.iter().any(|(existing, _)| *existing == ip)
            {
                out.push((ip, a.index));
            }
        }
    }
    out
}

/// rc.144 — force outbound datagrams on `sock` out the interface with OS index
/// `ifindex` via Windows `IP_UNICAST_IF`. Binding the source IP (rc.143) sets
/// the address but NOT the egress NIC on Windows (the "weak host model" — the
/// routing table picks the NIC), so a full-tunnel VPN's default route still
/// steals egress and same-WiFi direct oscillates (field: 4-7ms when it wins the
/// race, timeouts otherwise). `IP_UNICAST_IF` pins the NIC deterministically —
/// the Windows equivalent of `SO_BINDTODEVICE`. Best-effort: warns + continues
/// (a clean host routes fine, and the source-IP bind still helps).
#[cfg(all(windows, feature = "overlay-l3"))]
pub fn force_egress_interface(sock: &tokio::net::UdpSocket, ifindex: u32) {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{IPPROTO_IP, SOCKET, setsockopt};
    // IP_UNICAST_IF = 31. For IPv4 the value is the interface index in NETWORK
    // byte order (the classic gotcha — IPv6's IPV6_UNICAST_IF uses host order).
    const IP_UNICAST_IF: i32 = 31;
    let optval: u32 = ifindex.to_be();
    let ret = unsafe {
        setsockopt(
            sock.as_raw_socket() as SOCKET,
            IPPROTO_IP,
            IP_UNICAST_IF,
            (&optval as *const u32).cast::<u8>(),
            std::mem::size_of::<u32>() as i32,
        )
    };
    if ret == 0 {
        tracing::info!(
            ifindex,
            "overlay: pinned direct-socket egress to interface (IP_UNICAST_IF)"
        );
    } else {
        tracing::warn!(
            ifindex,
            "overlay: IP_UNICAST_IF failed; egress may follow the VPN default route"
        );
    }
}

/// No-op off Windows / without the WinSock bindings — the interface-bound
/// socket (rc.143) is the portable part; egress pinning is Windows-specific.
#[cfg(not(all(windows, feature = "overlay-l3")))]
pub fn force_egress_interface(_sock: &tokio::net::UdpSocket, _ifindex: u32) {}

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

/// NAT-traversal Phase A — opt-in gate for the **direct-to-public** carrier
/// tier (`ROOMLER_NODE_OVERLAY_PUBLIC_DIRECT`; legacy `ROOMLER_AGENT_…` alias
/// honoured — see [`crate::env::node_env`]). **Default OFF** until
/// field-proven, mirroring the QUIC gate's arc (CC8 in the NAT-traversal
/// plan). Gates the whole tier: dialing a peer's public endpoint, AND the
/// accept side (the runtime only wires the inbound-handshake receiver when this
/// is on). The accept path doubles as a roaming fix for restarted same-LAN
/// peers, but it rides this flag too so the fleet default stays byte-identical
/// until the tier is field-proven per-host.
pub fn public_direct_enabled() -> bool {
    match crate::env::node_env("OVERLAY_PUBLIC_DIRECT") {
        Some(v) => {
            let t = v.trim();
            t.eq_ignore_ascii_case("1")
                || t.eq_ignore_ascii_case("true")
                || t.eq_ignore_ascii_case("yes")
                || t.eq_ignore_ascii_case("on")
        }
        None => false,
    }
}

/// NAT-traversal Phase B — opt-in gate for the **srflx** carrier tier
/// (`ROOMLER_NODE_OVERLAY_SRFLX`; legacy `ROOMLER_AGENT_…` alias honoured).
/// **Default OFF** per CC8, independent of the public-direct gate. Turns on the
/// whole srflx tier: gathering + advertising this node's own server-reflexive
/// candidates (via STUN), AND dialing a peer's advertised srflx (a 1:1/cone-NAT
/// node whose NIC IP is private). Reuses Phase A's `public_sock` + demux +
/// authenticated-inbound accept, so the egress socket + inbound receiver are
/// wired whenever EITHER tier is on.
pub fn srflx_enabled() -> bool {
    match crate::env::node_env("OVERLAY_SRFLX") {
        Some(v) => {
            let t = v.trim();
            t.eq_ignore_ascii_case("1")
                || t.eq_ignore_ascii_case("true")
                || t.eq_ignore_ascii_case("yes")
                || t.eq_ignore_ascii_case("on")
        }
        None => false,
    }
}

/// Phase A — a globally-routable IPv4: the address classes that can never be
/// dialled across the internet are excluded (RFC1918 private, loopback,
/// link-local, CGNAT/overlay `100.64/10`, `0/8`, multicast `224/4`, and
/// `240/4` incl. broadcast). v4-only by design — v6 exit egress rides the v4
/// carrier (CC7). NB the TEST-NET ranges (`203.0.113.0/24` etc.) are
/// deliberately NOT excluded: they never appear on real NICs and double as
/// "public" space in unit fixtures.
pub fn is_public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || is_cgnat(ip)
        || o[0] == 0
        || o[0] >= 240)
}

/// Pick the first **public** `ip:port` from a peer's candidate bucket — used
/// for BOTH the Phase A public-NIC tier (the netmap's `lan_endpoints`, the
/// peer's NIC holding a public IP, dialable without STUN) and the Phase B srflx
/// tier (`srflx_endpoints`, the peer's public NAT mapping learned via STUN).
/// Either way the address is globally routable, so the same public dial path
/// (over `public_sock`) applies. Candidates equal to one of OUR OWN interface
/// IPs are skipped (a same-host / stale record can't be a peer dial target; a
/// genuinely same-subnet peer was already taken by the LAN tier, which runs
/// first). `None` → the caller falls through to the next tier or the relay.
pub fn pick_public_endpoint(my_ips: &[Ipv4Addr], candidates: &[String]) -> Option<SocketAddr> {
    for ep in candidates {
        if let Ok(SocketAddr::V4(sa)) = ep.trim().parse::<SocketAddr>()
            && is_public_v4(*sa.ip())
            && !my_ips.contains(sa.ip())
        {
            return Some(SocketAddr::V4(sa));
        }
    }
    None
}

/// Same-/24 test: two IPv4s share the top 24 bits. A strong, conservative
/// signal of same-L2-segment reachability for home/office LANs (good enough
/// for v1; a netmask-aware check is a refinement).
pub fn same_subnet_24(a: Ipv4Addr, b: Ipv4Addr) -> bool {
    let (a, b) = (a.octets(), b.octets());
    a[0] == b[0] && a[1] == b[1] && a[2] == b[2]
}

/// From a peer's advertised `endpoints` (host/srflx/relay strings), pick the
/// first that is a directly-dialable host endpoint **on one of our LANs** —
/// i.e. an `IP:port` whose IP shares a /24 with one of our interface IPs.
/// Returns `(our matching interface IP, the peer's endpoint)` so the caller can
/// send from the socket bound to THAT interface (rc.143 — binding to the
/// interface forces egress out the right NIC, so a same-subnet peer is reached
/// over the LAN even when a full-tunnel VPN has hijacked the default route).
/// `None` if the peer advertised no same-subnet endpoint (→ caller falls back
/// to the relay).
pub fn pick_same_subnet_endpoint(
    my_ips: &[Ipv4Addr],
    endpoints: &[String],
) -> Option<(Ipv4Addr, SocketAddr)> {
    for ep in endpoints {
        // Tolerate scheme-ish prefixes defensively; we only emit bare IP:port.
        let raw = ep.trim();
        if let Ok(SocketAddr::V4(sa)) = raw.parse::<SocketAddr>()
            && is_usable_lan_ipv4(*sa.ip())
            && let Some(local) = my_ips.iter().find(|me| same_subnet_24(**me, *sa.ip()))
        {
            return Some((*local, SocketAddr::V4(sa)));
        }
    }
    None
}

/// Phase B — parse a STUN endpoint from a `stun:` / `stuns:` URL (or a bare
/// `host:port`) **when the host is an IPv4 literal**. Strips the scheme and any
/// `?transport=…` / `#…` suffix. Returns `None` for a hostname (the caller
/// resolves those via DNS — this stays sync + allocation-light) or a
/// malformed / IPv6 value (v4-only, CC7). Coturn workers double as STUN
/// servers, so a `turn:` URL's host also works if the scheme is stripped first.
pub fn parse_stun_url(url: &str) -> Option<SocketAddr> {
    let s = url.trim();
    let s = s
        .strip_prefix("stun:")
        .or_else(|| s.strip_prefix("stuns:"))
        .or_else(|| s.strip_prefix("turn:"))
        .or_else(|| s.strip_prefix("turns:"))
        .unwrap_or(s);
    // Drop a `?transport=udp` query or `#frag`.
    let s = s.split(['?', '#']).next().unwrap_or(s);
    match s.parse::<SocketAddr>() {
        Ok(sa @ SocketAddr::V4(_)) => Some(sa),
        _ => None,
    }
}

/// Phase B — discover this node's **server-reflexive** candidates by querying
/// `stun_server` on EACH of its interface sockets. The query MUST ride the same
/// socket the overlay traffic will later use, or the NAT mapping won't match
/// (see [`crate::transport::stun`]) — so this takes the live `DirectCtx`
/// sockets and MUST run BEFORE their demux recv loops start (else the STUN
/// response races the loop's `recv`). Returns the deduped set of **public**
/// srflx `ip:port` strings to advertise; a socket whose query fails, times out,
/// or maps to a non-public address (STUN server on the LAN, a hairpin) is
/// skipped. v4-only.
pub async fn gather_srflx(
    socks: &[(Ipv4Addr, Arc<UdpSocket>)],
    stun_server: SocketAddr,
    attempt_timeout: Duration,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (_ip, sock) in socks {
        match crate::transport::stun::srflx_query(sock, stun_server, attempt_timeout).await {
            Ok(SocketAddr::V4(srflx)) if is_public_v4(*srflx.ip()) => {
                let ep = SocketAddr::V4(srflx).to_string();
                if !out.contains(&ep) {
                    out.push(ep);
                }
            }
            Ok(other) => {
                tracing::debug!(%other, "overlay: srflx candidate not public — skipping");
            }
            Err(e) => {
                tracing::debug!(%e, "overlay: srflx query failed on a socket — skipping");
            }
        }
    }
    out
}

/// Phase B — resolve the FIRST usable STUN server from the netmap's `stun_urls`
/// to a concrete v4 `SocketAddr`. An IP-literal URL is parsed synchronously
/// ([`parse_stun_url`], no DNS); a hostname URL (the fleet's
/// `stun:coturn.roomler.ai:3478`) is resolved via DNS and the first IPv4 answer
/// taken (v4-only, CC7). Tries each URL in order; `None` if none resolve to an
/// IPv4 endpoint. Any single reachable STUN worker suffices — srflx doesn't need
/// the coturn worker-pinning that the relay hairpin does.
pub async fn resolve_stun_server(stun_urls: &[String]) -> Option<SocketAddr> {
    for url in stun_urls {
        // Fast path: an IP literal (or already-resolved worker) needs no DNS.
        if let Some(sa) = parse_stun_url(url) {
            return Some(sa);
        }
        // Hostname → DNS. Strip the scheme + any `?transport` / `#frag`, keep
        // the `host:port` `lookup_host` needs.
        let s = url.trim();
        let s = s
            .strip_prefix("stun:")
            .or_else(|| s.strip_prefix("stuns:"))
            .or_else(|| s.strip_prefix("turn:"))
            .or_else(|| s.strip_prefix("turns:"))
            .unwrap_or(s);
        let hostport = s.split(['?', '#']).next().unwrap_or(s);
        if let Ok(addrs) = lookup_host(hostport).await
            && let Some(v4) = addrs.into_iter().find(SocketAddr::is_ipv4)
        {
            return Some(v4);
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
        let me: [Ipv4Addr; 1] = ["192.168.68.103".parse().unwrap()];
        // Mixed endpoint list: a far srflx, the relay, and the LAN host.
        let eps = vec![
            "37.63.112.129:49358".to_string(),  // srflx (different /24) — skip
            "94.130.141.74:3478".to_string(),   // relay — skip
            "192.168.68.110:51820".to_string(), // same /24 — pick this
        ];
        let got = pick_same_subnet_endpoint(&me, &eps).unwrap();
        assert_eq!(
            got,
            (
                "192.168.68.103".parse::<Ipv4Addr>().unwrap(),
                "192.168.68.110:51820".parse::<SocketAddr>().unwrap()
            )
        );

        // No same-subnet endpoint → None (caller uses relay).
        let only_far = vec!["37.63.112.129:49358".to_string()];
        assert!(pick_same_subnet_endpoint(&me, &only_far).is_none());

        // A same-subnet but CGNAT endpoint is rejected.
        let cgnat = vec!["100.64.0.110:51820".to_string()];
        assert!(pick_same_subnet_endpoint(&["100.64.0.103".parse().unwrap()], &cgnat).is_none());
    }

    #[test]
    fn public_v4_classification() {
        let public = ["5.9.157.226", "94.130.141.98", "203.0.113.9", "8.8.8.8"];
        for p in public {
            assert!(is_public_v4(p.parse().unwrap()), "{p} must classify public");
        }
        let not_public = [
            "192.168.68.103", // RFC1918
            "10.16.6.34",     // RFC1918
            "172.16.0.1",     // RFC1918
            "127.0.0.1",      // loopback
            "169.254.1.2",    // link-local
            "100.64.0.1",     // CGNAT / overlay
            "0.0.0.0",        // unspecified
            "0.1.2.3",        // 0/8
            "224.0.0.1",      // multicast
            "240.0.0.1",      // 240/4 reserved
            "255.255.255.255",
        ];
        for p in not_public {
            assert!(!is_public_v4(p.parse().unwrap()), "{p} must NOT be public");
        }
    }

    #[test]
    fn picks_first_public_endpoint_skipping_private_and_self() {
        let my_ips: [Ipv4Addr; 2] = [
            "94.130.141.98".parse().unwrap(),
            "192.168.150.1".parse().unwrap(),
        ];
        // Peer join bucket: its LAN address, then its public NIC address.
        let eps = vec![
            "192.168.7.23:41000".to_string(), // peer's private LAN — not dialable x-net
            "5.9.157.226:41234".to_string(),  // peer's public NIC — pick this
        ];
        assert_eq!(
            pick_public_endpoint(&my_ips, &eps),
            Some("5.9.157.226:41234".parse().unwrap())
        );

        // Our OWN public IP in a peer record is never a dial target.
        let self_ep = vec!["94.130.141.98:41000".to_string()];
        assert!(pick_public_endpoint(&my_ips, &self_ep).is_none());

        // All-private bucket → None (NAT'd peer; passive/relay handles it).
        let private_only = vec![
            "192.168.7.23:41000".to_string(),
            "10.0.0.5:41001".to_string(),
        ];
        assert!(pick_public_endpoint(&my_ips, &private_only).is_none());
    }

    #[test]
    fn gather_lan_ips_returns_only_usable_uniques() {
        // Exercises the real if-addrs enumeration on this host/CI runner. We
        // can't assert a specific set (host-dependent), only the invariants:
        // every gathered IP is usable, and there are no duplicates.
        let ips = gather_lan_ips();
        for ip in &ips {
            assert!(
                is_usable_lan_ipv4(*ip),
                "gather returned a non-usable IP: {ip}"
            );
        }
        let mut deduped = ips.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(
            deduped.len(),
            ips.len(),
            "gather_lan_ips returned duplicates"
        );
    }

    #[test]
    fn multi_homed_host_matches_on_the_right_interface() {
        // rc.132 regression guard: PC50045's bug. The node is multi-homed —
        // corporate Ethernet 172.30.x (the default route) + Wi-Fi 192.168.68.x.
        // The peer is on the Wi-Fi; we must match the 192.168.68 endpoint even
        // though 172.30 is "primary".
        let my_ips: [Ipv4Addr; 2] = [
            "172.30.239.96".parse().unwrap(), // corporate Ethernet (default route)
            "192.168.68.103".parse().unwrap(), // Wi-Fi (where the peer lives)
        ];
        // The peer (NEO16) advertises only ITS interfaces — a far srflx and its
        // Wi-Fi host. We must match the Wi-Fi endpoint against our SECONDARY
        // (non-default-route) Wi-Fi IP — the rc.131 connect-trick advertised
        // only 172.30 and so never matched.
        let peer_eps = vec![
            "37.63.112.129:49358".to_string(),  // peer srflx (far) — skip
            "192.168.68.110:58307".to_string(), // peer Wi-Fi — same /24 as our .103
        ];
        let got = pick_same_subnet_endpoint(&my_ips, &peer_eps).unwrap();
        assert_eq!(
            got,
            (
                "192.168.68.103".parse::<Ipv4Addr>().unwrap(),
                "192.168.68.110:58307".parse::<SocketAddr>().unwrap()
            )
        );
    }

    #[test]
    fn parse_stun_url_handles_schemes_and_rejects_hostnames() {
        let want: SocketAddr = "5.9.157.221:3478".parse().unwrap();
        assert_eq!(parse_stun_url("stun:5.9.157.221:3478"), Some(want));
        assert_eq!(parse_stun_url("stuns:5.9.157.221:3478"), Some(want));
        assert_eq!(
            parse_stun_url("turn:5.9.157.221:3478?transport=udp"),
            Some(want)
        );
        assert_eq!(parse_stun_url("5.9.157.221:3478"), Some(want));
        assert_eq!(parse_stun_url("  stun:5.9.157.221:3478  "), Some(want));
        // Hostnames need async DNS → the sync parser declines (caller resolves).
        assert_eq!(parse_stun_url("stun:coturn.roomler.ai:3478"), None);
        // IPv6 is out of scope (v4-only cascade).
        assert_eq!(parse_stun_url("stun:[2a01:4f8::2]:3478"), None);
        assert_eq!(parse_stun_url("garbage"), None);
    }

    /// Minimal STUN Binding Success carrying an XOR-MAPPED-ADDRESS (IPv4), so
    /// the gather test needs no real STUN server. Mirrors RFC 5389 §15.2.
    fn stun_success(txn: [u8; 12], ip: [u8; 4], port: u16) -> Vec<u8> {
        const COOKIE: u32 = 0x2112_A442;
        let cookie = COOKIE.to_be_bytes();
        let xport = port ^ ((COOKIE >> 16) as u16);
        let mut r = Vec::new();
        r.extend_from_slice(&0x0101u16.to_be_bytes()); // Binding Success
        r.extend_from_slice(&12u16.to_be_bytes()); // one 12-byte attribute
        r.extend_from_slice(&cookie);
        r.extend_from_slice(&txn);
        r.extend_from_slice(&0x0020u16.to_be_bytes()); // XOR-MAPPED-ADDRESS
        r.extend_from_slice(&8u16.to_be_bytes());
        r.push(0);
        r.push(0x01); // family IPv4
        r.extend_from_slice(&xport.to_be_bytes());
        r.extend_from_slice(&[
            ip[0] ^ cookie[0],
            ip[1] ^ cookie[1],
            ip[2] ^ cookie[2],
            ip[3] ^ cookie[3],
        ]);
        r
    }

    /// Spawn a fake STUN server that answers every Binding Request with a
    /// success carrying `reply_ip:reply_port`. Returns its addr + the task
    /// handle (kept alive by the caller for the test's duration).
    async fn fake_stun_server(
        reply_ip: [u8; 4],
        reply_port: u16,
    ) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let srv = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr = srv.local_addr().unwrap();
        let h = tokio::spawn(async move {
            let mut buf = [0u8; 512];
            while let Ok((n, from)) = srv.recv_from(&mut buf).await {
                if n >= 20 {
                    let txn: [u8; 12] = buf[8..20].try_into().unwrap();
                    let _ = srv
                        .send_to(&stun_success(txn, reply_ip, reply_port), from)
                        .await;
                }
            }
        });
        (addr, h)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn gather_srflx_captures_public_filters_private_and_dead() {
        // A PUBLIC srflx reply is captured.
        let (pub_srv, _h1) = fake_stun_server([203, 0, 113, 9], 40000).await;
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let socks = vec![("127.0.0.1".parse().unwrap(), sock)];
        let got = gather_srflx(&socks, pub_srv, Duration::from_millis(500)).await;
        assert_eq!(got, vec!["203.0.113.9:40000".to_string()]);

        // A PRIVATE srflx (STUN on the LAN / hairpin) is filtered out.
        let (priv_srv, _h2) = fake_stun_server([192, 168, 1, 5], 41000).await;
        let sock2 = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let socks2 = vec![("127.0.0.1".parse().unwrap(), sock2)];
        assert!(
            gather_srflx(&socks2, priv_srv, Duration::from_millis(500))
                .await
                .is_empty()
        );

        // A dead STUN server yields no candidates (fast timeout, no hang).
        let dead: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let sock3 = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let socks3 = vec![("127.0.0.1".parse().unwrap(), sock3)];
        assert!(
            gather_srflx(&socks3, dead, Duration::from_millis(150))
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn resolve_stun_server_prefers_ip_literals_and_skips_bad_entries() {
        let want: SocketAddr = "5.9.157.221:3478".parse().unwrap();
        // An IP-literal URL resolves synchronously — no DNS.
        assert_eq!(
            resolve_stun_server(&["stun:5.9.157.221:3478".to_string()]).await,
            Some(want)
        );
        // Empty → None (srflx tier inert).
        assert_eq!(resolve_stun_server(&[]).await, None);
        // A malformed leading entry (no `host:port`, so `lookup_host` errors
        // immediately without network I/O) is skipped → the usable IP literal
        // behind it wins.
        assert_eq!(
            resolve_stun_server(&[
                "not-a-host-port".to_string(),
                "stun:5.9.157.221:3478".to_string(),
            ])
            .await,
            Some(want)
        );
    }
}

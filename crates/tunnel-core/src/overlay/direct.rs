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

/// Built-in stable port for the direct sockets (see [`direct_port`]).
/// Deliberately NOT 41641 (Tailscale's WireGuard port — fleet hosts run both)
/// and not 51820 (kernel WireGuard's default).
pub const DEFAULT_DIRECT_PORT: u16 = 41648;

/// Stable UDP port for the overlay's direct sockets
/// (`ROOMLER_NODE_OVERLAY_DIRECT_PORT`; config key `overlay_direct_port`).
///
/// Per-interface LAN sockets bind `(iface_ip, port)`; the public/srflx
/// dialer binds `(0.0.0.0, port+1)` (a wildcard bind on the SAME port as a
/// specific-IP bind fails without SO_REUSEADDR). `0` = ephemeral ports, the
/// pre-rc.307 behavior.
///
/// Why a stable port: stateful corp firewalls (Check Point on pc50045)
/// GRANDFATHER UDP flows that predate the VPN's session table — direct
/// carriers established before the VPN connects keep working, but any
/// rebuild (agent update, control-WS reconnect on a server deploy) used to
/// bind fresh ephemeral ports, presenting a NEW 5-tuple the VPN then drops,
/// relay-locking the node until the next VPN-off window. With both fleet
/// ends on stable ports, a rebuilt carrier reproduces the SAME 5-tuple and
/// keeps riding the grandfathered session (2026-08-05 field diagnosis:
/// 7/7 VPN-off rebuilds promoted LAN direct, 10/10 VPN-on rebuilds never).
///
/// Unparseable values fall back to the default (a typo must not silently
/// turn the feature off fleet-wide).
pub fn direct_port() -> u16 {
    match crate::env::node_env("OVERLAY_DIRECT_PORT") {
        Some(v) => match v.trim().parse::<u32>() {
            // 65535 excluded: the public dialer needs port+1.
            Ok(n) if n <= 65534 => n as u16,
            _ => DEFAULT_DIRECT_PORT,
        },
        None => DEFAULT_DIRECT_PORT,
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
    let filter = lan_iface_filter_enabled();
    if let Ok(addrs) = if_addrs::get_if_addrs() {
        for a in addrs {
            if let std::net::IpAddr::V4(ip) = a.ip()
                && is_usable_lan_ipv4(ip)
                && !out.iter().any(|(existing, _)| *existing == ip)
            {
                // rc.275 hygiene — skip virtual / host-only / other-VPN
                // interfaces (see `lan_iface_denied`). Field: pc50045
                // advertised its WSL vEthernet `172.31.176.1` (a host-only
                // NAT address no peer can ever reach) and its Check Point
                // VPN `172.30.x` as "LAN" endpoints, each with a pinned
                // per-interface socket — poisoning the same-/24 match for
                // every peer that happens to share those private ranges.
                if filter {
                    let info = a.index.and_then(iface_info);
                    let desc = info.as_ref().map(|(d, _)| d.as_str()).unwrap_or_default();
                    let hardware = info.as_ref().map(|(_, h)| *h);
                    // rc.281 — `hardware == Some(false)` fails CLOSED for VPN
                    // vendors the deny-list has never heard of (the list only
                    // grows after a field incident; the hardware bit needs no
                    // vendor knowledge). Caveat: NIC-teaming/VLAN child
                    // adapters also report non-hardware and would be skipped —
                    // rare on the fleet, benign direction (not advertised ⇒
                    // relay still works), and `overlay_lan_iface_filter=0`
                    // restores the unfiltered gather. `None` (lookup failed /
                    // non-Windows) keeps the name-only checks as the deciders.
                    if lan_iface_denied(&a.name, desc) || hardware == Some(false) {
                        tracing::debug!(
                            iface = %a.name, %ip, desc = %desc, hardware = ?hardware,
                            "overlay: LAN gather — skipping virtual/host-only interface"
                        );
                        continue;
                    }
                }
                out.push((ip, a.index));
            }
        }
    }
    out
}

/// rc.275 hygiene — gate for the LAN-gather virtual-interface filter
/// (`ROOMLER_NODE_OVERLAY_LAN_IFACE_FILTER`; legacy `ROOMLER_AGENT_…` alias
/// honoured — see [`crate::env::node_env`]). Default **ON**; set
/// `0`/`false`/`no`/`off` to restore the unfiltered pre-rc.275 gather if the
/// deny-list ever misclassifies a real NIC in the field (the failure mode is
/// benign either way — a skipped interface just isn't advertised, and the
/// relay path still works).
pub fn lan_iface_filter_enabled() -> bool {
    match crate::env::node_env("OVERLAY_LAN_IFACE_FILTER") {
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

/// rc.275 hygiene — `true` when an interface must NOT be advertised as a LAN
/// endpoint (nor given a pinned per-interface direct socket): virtual
/// switches, host-only NATs, container bridges, and OTHER VPNs' adapters.
/// Their addresses are unreachable by any real peer, and advertising them
/// poisons the same-/24 LAN match (rc.204 family). Matched case-insensitively
/// against the interface NAME and (Windows) the driver DESCRIPTION — the
/// Check Point adapter's friendly name is just "Ethernet"; only its
/// description ("Check Point Virtual Network Adapter …") gives it away.
/// webrtc-ice solves this with an application `interface_filter` callback;
/// the overlay gathers directly, so the deny-list lives here. Pure.
pub fn lan_iface_denied(name: &str, description: &str) -> bool {
    // Substring matches — vendor-grade names/descriptions (either field).
    const SUBSTRING: &[&str] = &[
        "vethernet", // Hyper-V "vEthernet (WSL …)" / "vEthernet (Default Switch)"
        "hyper-v",   // "Hyper-V Virtual Ethernet Adapter"
        "wsl",       // WSL switch names
        "virtual",   // "… Virtual Network Adapter" (Check Point, VMware, …)
        "vmware",
        "vmnet",
        "virtualbox",
        "vbox",
        "wintun", // stale/orphaned Wintun stubs ("Wintun Userspace Tunnel")
        "wireguard",
        "tailscale",
        "zerotier",
        "openvpn",
        "ovpn",
        "tap-windows",
        "check point",
        "pangp",
        "fortissl",
        "juniper",
        "loopback",
    ];
    // Prefix matches on the NAME — kernel/driver naming conventions
    // (Linux/macOS/BSD: docker0, veth*, virbr0, br-*, tun0, tap0, utun3,
    // wg0, ppp0, bridge100). Prefix (not substring) so real adapters whose
    // names merely CONTAIN these stay allowed.
    const NAME_PREFIX: &[&str] = &[
        "docker", "veth", "virbr", "br-", "bridge", "tun", "tap", "utun", "wg", "ppp", "roomler",
    ];
    let n = name.to_ascii_lowercase();
    let d = description.to_ascii_lowercase();
    SUBSTRING.iter().any(|s| n.contains(s) || d.contains(s))
        || NAME_PREFIX.iter().any(|p| n.starts_with(p))
}

/// Windows — the interface DESCRIPTION + hardware bit for an OS ifindex via
/// `GetIfEntry2`. The description ("Check Point Virtual Network Adapter For
/// Endpoint VPN Client") feeds the deny-list — the friendly name alone can't
/// classify these (the Check Point adapter is named just "Ethernet"). The
/// `HardwareInterface` flag (rc.281) is the STRUCTURAL signal the deny-list
/// can't be: every virtual adapter — Wintun, Hyper-V vSwitch, any VPN vendor
/// we've never heard of — reports `false`, while physical Ethernet/Wi-Fi
/// report `true`, so unknown vendors now fail CLOSED instead of open.
/// `None` on lookup failure (then the name-only checks still apply).
#[cfg(all(windows, feature = "overlay-l3"))]
fn iface_info(ifindex: u32) -> Option<(String, bool)> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{GetIfEntry2, MIB_IF_ROW2};
    // SAFETY: zeroed row + InterfaceIndex is the documented lookup-by-index
    // call shape; GetIfEntry2 fills the row on NO_ERROR (0).
    unsafe {
        let mut row: MIB_IF_ROW2 = std::mem::zeroed();
        row.InterfaceIndex = ifindex;
        if GetIfEntry2(&mut row) != 0 {
            return None;
        }
        let len = row
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(row.Description.len());
        // `InterfaceAndOperStatusFlags` is a packed C bitfield byte;
        // `HardwareInterface` is its FIRST member, and MSVC allocates
        // bitfields low-to-high ⇒ bit 0.
        let hardware = row.InterfaceAndOperStatusFlags._bitfield & 0x01 != 0;
        Some((String::from_utf16_lossy(&row.Description[..len]), hardware))
    }
}

/// No-op off Windows — Linux/macOS interface NAMES already follow the
/// conventions the prefix list catches (docker0, veth*, utun*, wg*…).
#[cfg(not(all(windows, feature = "overlay-l3")))]
fn iface_info(_ifindex: u32) -> Option<(String, bool)> {
    None
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

/// Gate for **bind-to-interface-by-route** LAN-carrier egress selection
/// (`ROOMLER_NODE_OVERLAY_BIND_BY_ROUTE`; legacy `ROOMLER_AGENT_…` alias
/// honoured). **Default OFF** until field-proven, mirroring the QUIC /
/// `public_direct` arc. When on, a LAN direct carrier's egress interface is
/// chosen per-destination from the OS route table (the connect()-trick,
/// [`os_src_ip_for`]) + [`classify_egress`], and the socket is re-pinned to
/// the CURRENT ifindex — instead of relying on the same-subnet heuristic and a
/// pin computed once at startup. This is Tailscale's `bindToInterfaceByRoute`
/// (net/netns) adapted to roomler: an on-link `/24` beats a full-tunnel VPN's
/// `/1` default, so a genuine same-LAN peer stays on the physical NIC even
/// under a corporate VPN, and a peer the OS routes elsewhere (VPN-captured)
/// falls to relay honestly instead of flapping a one-way "direct".
pub fn bind_by_route_enabled() -> bool {
    match crate::env::node_env("OVERLAY_BIND_BY_ROUTE") {
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

/// Gate for **VPN-bypass** carrier egress (`ROOMLER_NODE_OVERLAY_VPN_BYPASS`;
/// legacy `ROOMLER_AGENT_…` alias honoured). **Default OFF** opt-in. When on
/// (and an uplink ifindex is resolved), EVERY overlay underlay carrier socket
/// — the `public_sock`, the single-relay dialer, and the coturn TURN underlay —
/// has its egress pinned (`IP_UNICAST_IF`) to the host's real PHYSICAL uplink,
/// forcing the overlay's own transport out the physical NIC instead of a
/// full-tunnel corporate VPN's captured default route. Confirmed on ÖBB
/// pc50045 (2026-07-30): a Check Point full-tunnel VPN captured ALL egress
/// (`Find-NetRoute` → every dst via `172.30.x/Ethernet`), so its carriers rode
/// the VPN one-way; pinning to the physical Wi-Fi bypasses it. This is
/// Tailscale's `net/netns` "bind to the physical interface, not another VPN's
/// tunnel" applied to the whole underlay. Mirrors the `public_direct` opt-in
/// arc; flips default-ON after the pc50045 field-proof.
pub fn vpn_bypass_enabled() -> bool {
    match crate::env::node_env("OVERLAY_VPN_BYPASS") {
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

/// Operator-pinned physical-uplink OS interface index for [`vpn_bypass_enabled`]
/// (`ROOMLER_NODE_OVERLAY_UPLINK_IF` = a numeric ifindex, e.g. the Wi-Fi
/// adapter's index from `Get-NetIPInterface`). This explicit override
/// field-proves the bypass mechanism before auto-discovery of the physical
/// uplink beneath a captured VPN is built. `None` when unset/unparseable.
pub fn uplink_ifindex_override() -> Option<u32> {
    crate::env::node_env("OVERLAY_UPLINK_IF").and_then(|v| v.trim().parse::<u32>().ok())
}

/// The physical-uplink ifindex a carrier underlay socket should pin its egress
/// to, or `None` to leave egress on the OS default route (today's behaviour).
/// `Some` only when VPN-bypass is enabled AND an uplink ifindex is resolved
/// (currently the explicit [`uplink_ifindex_override`]; auto-discovery later).
pub fn vpn_bypass_ifindex() -> Option<u32> {
    if vpn_bypass_enabled() {
        uplink_ifindex_override()
    } else {
        None
    }
}

/// The OS's chosen egress for a LAN direct destination, classified against the
/// interfaces we hold a bound socket on. Pure output of [`classify_egress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Egress {
    /// The OS routes `dst` via an interface we hold a socket on (its source
    /// IP). Use that socket, re-pinned to its current ifindex.
    Use(Ipv4Addr),
    /// The OS routes `dst` back into the overlay's own CGNAT range (our TUN) —
    /// binding there would loop. Skip direct (the loop-guard).
    Loop,
    /// The OS routes `dst` via an interface we do NOT hold a LAN socket on
    /// (e.g. a full-tunnel VPN captured this destination). The same-subnet LAN
    /// carrier would be one-way → skip direct, fall to relay honestly.
    Foreign,
    /// The route query failed / gave no answer → caller keeps the pre-existing
    /// same-subnet behaviour (never worse than today).
    Unknown,
}

/// Classify the OS's chosen source IP for a LAN destination against the set of
/// interface IPs we hold a bound direct socket on. Pure (no I/O) so the
/// decision is unit-tested on synthetic data, exactly like
/// [`pick_same_subnet_endpoint`]. The loop-guard is [`is_cgnat`]: the overlay's
/// own TUN carries a `100.64.0.0/10` address, so an OS source-IP in that range
/// means the route resolves back into the overlay itself.
///
/// - `None` (query failed) → [`Egress::Unknown`].
/// - CGNAT source → [`Egress::Loop`].
/// - source ∈ `our_socket_ips` → [`Egress::Use`].
/// - any other source → [`Egress::Foreign`].
pub fn classify_egress(src_ip: Option<Ipv4Addr>, our_socket_ips: &[Ipv4Addr]) -> Egress {
    match src_ip {
        None => Egress::Unknown,
        Some(ip) if is_cgnat(ip) => Egress::Loop,
        Some(ip) if our_socket_ips.contains(&ip) => Egress::Use(ip),
        Some(_) => Egress::Foreign,
    }
}

/// The OS's chosen SOURCE IPv4 for reaching `dst` — the portable "which
/// interface would this packet leave from?" query (Tailscale's `connect()`
/// trick, `net/netns`): bind a throwaway UDP socket to `0.0.0.0:0`,
/// `connect(dst)` (which sends **no** packet — on a UDP socket it only sets the
/// default peer and makes the kernel resolve the route + assign a local
/// address), then read `local_addr()`. The kernel honours the full routing
/// table, so an on-link `/24` wins over a VPN's `/1` split-default and a genuine
/// same-LAN `dst` resolves to the physical NIC even under a full-tunnel VPN.
/// Works on every platform (including Windows, where it complements
/// `IP_UNICAST_IF`). `None` on any error → caller falls back to the same-subnet
/// heuristic.
pub async fn os_src_ip_for(dst: SocketAddr) -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.ok()?;
    sock.connect(dst).await.ok()?;
    match sock.local_addr().ok()? {
        SocketAddr::V4(a) if !a.ip().is_unspecified() => Some(*a.ip()),
        _ => None,
    }
}

/// The CURRENT OS interface index for one of our LAN interface IPs, re-read
/// from [`gather_lan_interfaces`] at call time so the egress pin reflects the
/// live interface table (a VPN connect can add interfaces / renumber). `None`
/// if the IP is no longer present or `if-addrs` supplies no index.
pub fn ifindex_for(ip: Ipv4Addr) -> Option<u32> {
    gather_lan_interfaces()
        .into_iter()
        .find(|(i, _)| *i == ip)
        .and_then(|(_, ix)| ix)
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

/// Gate for **make-before-break** carrier upgrades
/// (`ROOMLER_NODE_OVERLAY_MBB`; legacy `ROOMLER_AGENT_…` alias honoured).
/// **Default ON since rc.210** — field-proven 2026-07-25 on the netns NAT lab
/// (mars↔zeus, the false-same-/24-LAN-match freeze scenario): MBB=1 held the
/// relay while a doomed direct upgrade was probed then dropped it ("kept relay
/// (no stall)"), where MBB=0 tore the relay down ("upgrading relay peer to
/// direct LAN carrier"). Disable per-host with `ROOMLER_NODE_OVERLAY_MBB=0`
/// (kill-switch): only an explicit `0`/`false`/`no`/`off` turns it back off;
/// unset / truthy / anything else keeps the default ON.
///
/// A relay→direct UPGRADE installs the candidate direct carrier as a SHADOW
/// PROBE (its own `Tunn`, in `WgDevice::probes`) while the working relay keeps
/// routing, and only cuts over once the probe's handshake latches (proof the
/// direct path works both ways). If the probe never latches within the tier's
/// deadline it is dropped and the relay is untouched — so a peer that can only
/// ever relay (same-NAT AP-isolation / no hairpin) never suffers the ~15–38 s
/// freeze the old break-before-make upgrade caused every re-upgrade tick (it
/// tore the relay down, gambled on an unreachable direct path, then
/// re-established relay). `=0` ⇒ the pre-rc.208 destructive upgrade, byte-for-
/// byte. Covers BOTH the outbound upgrade AND the inbound-accept path
/// (`handle_direct_inbound`) since rc.209.
pub fn make_before_break_enabled() -> bool {
    match crate::env::node_env("OVERLAY_MBB") {
        Some(v) => {
            let t = v.trim();
            // Explicit kill-switch only; everything else keeps the default ON.
            !(t.eq_ignore_ascii_case("0")
                || t.eq_ignore_ascii_case("false")
                || t.eq_ignore_ascii_case("no")
                || t.eq_ignore_ascii_case("off"))
        }
        None => true,
    }
}

/// NAT-traversal Phase B/C — gate for the **srflx** carrier tier
/// (`ROOMLER_NODE_OVERLAY_SRFLX`; legacy `ROOMLER_AGENT_…` alias honoured).
/// **Default ON** since 2026-07-20 (field-proven: a cone↔cone pair hole-punches
/// to a DIRECT carrier — mars↔zeus netns lab, 0% loss, ~0.6 ms, half the relay
/// RTT). Turns on the whole srflx tier: gathering + advertising this node's own
/// server-reflexive candidates (via STUN), AND dialing a peer's advertised srflx
/// (a 1:1/cone-NAT node whose NIC IP is private). The tier FALLS THROUGH — a
/// failed/both-symmetric punch degrades to the relay tier — so default-ON only
/// adds a direct-connect fast path, never removes reachability. Set the env to
/// `0`/`false`/`no`/`off` to disable.
pub fn srflx_enabled() -> bool {
    match crate::env::node_env("OVERLAY_SRFLX") {
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

/// NAT-traversal Phase D — gate for the **single-relay** carrier tier
/// (`ROOMLER_NODE_OVERLAY_RELAY_SINGLE`; legacy `ROOMLER_AGENT_…` alias
/// honoured). **Default ON** since 2026-07-20. When on (and both ends advertise
/// the capability), a relay-tier pair uses ONE coturn allocation — the ANCHOR
/// (smaller pubkey) allocates + runs the QUIC server + permits the dialer's IP;
/// the DIALER (larger pubkey) sends raw UDP to the anchor's relayed address as a
/// plain TURN peer (no allocation). This avoids the both-allocate coturn hairpin
/// (the open REKEY_TIMEOUT relay bug) and carries symmetric NAT (permissions are
/// IP-only). Field-proven in the full runtime (sym↔sym mars↔zeus netns lab,
/// 2026-07-20: `single_relay=true` → QUIC-over-TURN up both ways → WG 0% loss);
/// default-ON is net-positive since both-allocate was already broken cross-NAT.
/// v1 serves BOTH-UDP-OK pairs; a UDP-blocked dialer (raw UDP can't reach
/// coturn) stays dark on the relay tier — the documented v1 limitation, no worse
/// than the broken both-allocate it replaces. Set `0`/`false`/`no`/`off` to
/// disable.
pub fn relay_single_enabled() -> bool {
    match crate::env::node_env("OVERLAY_RELAY_SINGLE") {
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

/// rc.276 (B-probe) — force ALL overlay coturn allocations onto the
/// **TURNS/TCP (TLS) tier** (`ROOMLER_NODE_OVERLAY_RELAY_TLS`; legacy
/// `ROOMLER_AGENT_…` alias honoured). **Default OFF** opt-in (positive truthy
/// only), mirroring `public_direct_enabled` — this is the field-diagnostic
/// twin of remote-control's `ROOMLER_AGENT_ICE_RELAY_TCP`: the WebRTC
/// screen-share survives corp endpoint VPNs via `turns:coturn:443?tcp`
/// (real TLS + SNI, OS-native trust — indistinguishable from HTTPS), while
/// the overlay's Tier-2 UDP allocate "succeeds" and then runs silently
/// one-way, so the TLS tier never engages on its own. Forcing it answers
/// the gating question for the auto-demotion follow-up: does a WG handshake
/// complete over a TLS-TURN carrier on the affected host at all? (DERP —
/// also WG-in-TLS — did NOT survive there, so this is a genuine experiment,
/// not a foregone conclusion.)
///
/// Side effect: while forced, the node also advertises
/// `supports_relay_single=false` and turns its local single-relay flag off —
/// the raw-UDP DIALER role is exactly the flow shape the affected hosts
/// can't send, and both ends must compute the same strategy (the peer reads
/// our capability from the join, so the veto stays pair-symmetric).
pub fn relay_tls_forced() -> bool {
    match crate::env::node_env("OVERLAY_RELAY_TLS") {
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

/// Phase D (DERP) — is the pubkey-addressed `/derp` relay carrier ENABLED?
/// (`ROOMLER_NODE_OVERLAY_DERP`; legacy `ROOMLER_AGENT_…` alias honoured.)
/// **Default ON** since 2026-07-21 (field-proven). DERP is the last-resort
/// carrier for two BOTH-UDP-blocked peers (a strict corp firewall that permits
/// only TCP/TLS-443), which no other tier can serve; both peers dial OUT to the
/// relay over WSS:443 and WG rides end-to-end. Only CHOSEN when both ends
/// advertise `supports_derp` AND both are UDP-blocked (the single-relay
/// `(false,false)` arm), so a UDP-capable pair never touches it — default-ON
/// just means an overlay node keeps a `/derp` WS available in case a
/// both-UDP-blocked peer appears. Field-proven 2026-07-21 (mars↔zeus netns,
/// both UDP+coturn-TCP-blocked → WG over `/derp` at 0% loss, ~2.7 ms). Set
/// `0`/`false`/`no`/`off` to disable. (Follow-up: open the `/derp` WS lazily —
/// only when this node is itself UDP-blocked — so UDP-capable nodes don't hold
/// an idle WS.)
pub fn derp_enabled() -> bool {
    match crate::env::node_env("OVERLAY_DERP") {
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

/// Phase D — should this node GATHER + ADVERTISE its own srflx candidates? True
/// when the srflx-direct tier is on OR single-relay is on. Single-relay needs it
/// even with srflx-direct OFF: a single-relay DIALER (larger pubkey) runs no
/// coturn allocation, so the ANCHOR can only permit its inbound by IP — and it
/// learns that IP from the dialer's advertised srflx. So a node opting into
/// single-relay MUST advertise its srflx (it may be the dialer for any
/// larger-pubkey peer), else the anchor withholds forever. This gates ONLY the
/// gather+advertise machinery — it does NOT turn on the srflx-direct DIAL tier
/// (that stays [`srflx_enabled`], so single-relay can be field-tested in
/// isolation: srflx advertised, direct-dial off ⇒ pairs fall to the relay tier).
pub fn srflx_gather_active() -> bool {
    srflx_enabled() || relay_single_enabled()
}

/// Phase C — the srflx keepalive/re-gather interval in seconds
/// (`ROOMLER_NODE_OVERLAY_SRFLX_KEEPALIVE_SECS`, default 20). The task re-runs a
/// STUN Binding on the punch socket every interval to (a) hold the NAT mapping
/// open on an idle link and (b) detect + re-advertise a changed mapping. **`0`
/// disables the task entirely** — the startup gather still advertises once, but
/// there's no in-band refresh (the mapping then relies on WG keepalives for
/// active links only). A malformed value falls back to the default.
pub fn srflx_keepalive_secs() -> u64 {
    match crate::env::node_env("OVERLAY_SRFLX_KEEPALIVE_SECS") {
        Some(v) => v.trim().parse::<u64>().unwrap_or(20),
        None => 20,
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
///
/// Phase C — each candidate is returned WITH the interface socket it was
/// gathered on. The overlay must later DIAL a peer's srflx from the socket that
/// owns OUR advertised srflx (so our outbound INITs ride the same NAT mapping we
/// advertised, opening our filter toward the peer — the hole-punch); pairing the
/// candidate with its socket lets the caller pick that "punch socket". The first
/// pair is the punch socket (its candidate is advertised at index 0, which the
/// peer's dial-side picks first). Deduped by candidate string.
pub async fn gather_srflx(
    socks: &[(Ipv4Addr, Arc<UdpSocket>)],
    stun_server: SocketAddr,
    attempt_timeout: Duration,
) -> Vec<(String, Arc<UdpSocket>)> {
    let mut out: Vec<(String, Arc<UdpSocket>)> = Vec::new();
    for (_ip, sock) in socks {
        match crate::transport::stun::srflx_query(sock, stun_server, attempt_timeout).await {
            Ok(SocketAddr::V4(srflx)) if is_public_v4(*srflx.ip()) => {
                let ep = SocketAddr::V4(srflx).to_string();
                if !out.iter().any(|(e, _)| e == &ep) {
                    out.push((ep, sock.clone()));
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
pub async fn resolve_stun_server(stun_urls: &[String], exclude: &[Ipv4Addr]) -> Option<SocketAddr> {
    // Never STUN a coturn worker that is one of THIS host's own IPs: on the
    // fleet the coturn workers ARE the hosts (mars `.74`, jupiter `.221`, zeus
    // `.226`), so a co-located host STUNning its own worker hairpins on the
    // local host DNAT and gets no public mapping back → the node falsely reads
    // as UDP-blocked (empty srflx). Real clients are never co-located with
    // coturn, so this only ever prunes the fleet's self-referential target.
    let usable = |sa: &SocketAddr| !matches!(sa, SocketAddr::V4(v4) if exclude.contains(v4.ip()));
    for url in stun_urls {
        // Fast path: an IP literal (or already-resolved worker) needs no DNS.
        if let Some(sa) = parse_stun_url(url) {
            if usable(&sa) {
                return Some(sa);
            }
            continue;
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
            && let Some(v4) = addrs.into_iter().filter(SocketAddr::is_ipv4).find(usable)
        {
            return Some(v4);
        }
    }
    None
}

/// Phase C — resolve up to TWO DISTINCT STUN targets for the NAT-type probe.
/// The probe compares our mapped address as two different servers see it: same
/// ⇒ endpoint-independent mapping (cone — hole-punchable); different ⇒ symmetric
/// (address/port-dependent — the peer can't predict our port). Prefers two
/// DISTINCT IPs (the fleet resolves `coturn.roomler.ai` to several workers — the
/// strongest test, catching address-dependent mapping); else falls back to two
/// distinct ports on one IP (catches address-and-port-dependent mapping, the
/// common symmetric). v4 only. 0-2 results; fewer than 2 ⇒ the caller can't
/// classify (→ "unknown", stays optimistic and still attempts the punch).
pub async fn resolve_stun_targets(stun_urls: &[String], exclude: &[Ipv4Addr]) -> Vec<SocketAddr> {
    // Same self-referential-worker skip as `resolve_stun_server` (see its doc):
    // a fleet host co-located with a coturn worker must not probe against its
    // own IP, or the NAT-type probe hairpins on the local host DNAT.
    let usable = |sa: &SocketAddr| !matches!(sa, SocketAddr::V4(v4) if exclude.contains(v4.ip()));
    let mut all: Vec<SocketAddr> = Vec::new();
    for url in stun_urls {
        if let Some(sa) = parse_stun_url(url) {
            if usable(&sa) && !all.contains(&sa) {
                all.push(sa);
            }
            continue;
        }
        let s = url.trim();
        let s = s
            .strip_prefix("stun:")
            .or_else(|| s.strip_prefix("stuns:"))
            .or_else(|| s.strip_prefix("turn:"))
            .or_else(|| s.strip_prefix("turns:"))
            .unwrap_or(s);
        let hostport = s.split(['?', '#']).next().unwrap_or(s);
        if let Ok(addrs) = lookup_host(hostport).await {
            for a in addrs.filter(SocketAddr::is_ipv4) {
                if usable(&a) && !all.contains(&a) {
                    all.push(a);
                }
            }
        }
    }
    let mut out: Vec<SocketAddr> = Vec::new();
    if let Some(&first) = all.first() {
        out.push(first);
        // Prefer a DIFFERENT IP; else any other distinct endpoint (diff port).
        if let Some(&diff) = all
            .iter()
            .find(|a| a.ip() != first.ip())
            .or_else(|| all.iter().find(|&&a| a != first))
        {
            out.push(diff);
        }
    }
    out
}

/// Phase C — classify this node's NAT mapping by STUNning `sock` against TWO
/// distinct `targets`. Endpoint-INDEPENDENT mapping (the same public `ip:port`
/// from both) ⇒ `"cone"` (hole-punchable); a DIFFERENT mapping per target ⇒
/// `"symmetric"` (not punchable). `None` when there are fewer than two targets
/// or either query fails — the caller then advertises no NAT type and still
/// ATTEMPTS the punch ("unknown" is optimistic). MUST run on the punch socket
/// BEFORE its demux loop starts (same socket-read race as [`gather_srflx`]).
pub async fn probe_nat_type(
    sock: &UdpSocket,
    targets: &[SocketAddr],
    attempt_timeout: Duration,
) -> Option<&'static str> {
    if targets.len() < 2 {
        return None;
    }
    let a = crate::transport::stun::srflx_query(sock, targets[0], attempt_timeout)
        .await
        .ok()?;
    let b = crate::transport::stun::srflx_query(sock, targets[1], attempt_timeout)
        .await
        .ok()?;
    Some(if a == b { "cone" } else { "symmetric" })
}

/// Phase C — should we ATTEMPT a srflx hole-punch given both ends' NAT types?
/// Skip only when we're CONFIDENT it can't work: BOTH ends symmetric (neither
/// can predict the other's per-destination port). Any `None`/"unknown" stays
/// optimistic and attempts (the tight handshake deadline bounds a wasted try).
pub fn srflx_punch_worth_trying(mine: Option<&str>, peer: Option<&str>) -> bool {
    !(mine == Some("symmetric") && peer == Some("symmetric"))
}

/// P8 — is a srflx punch toward this peer a pointless HAIRPIN? `true` when
/// every srflx candidate the peer advertises maps to the SAME public IP as our
/// own srflx (both ends behind one NAT — the punch would need router
/// hairpinning, which consumer NATs rarely do) AND the peer also advertises a
/// LAN candidate on one of our subnets (the tier that actually works for a
/// same-site pair). Saves the futile 12 s attempt + its probe noise on every
/// re-upgrade tick. A same-IP pair WITHOUT a usable LAN candidate still tries
/// — on an isolated segment the hairpin may be all there is. Pure.
pub fn srflx_hairpin_pointless(
    my_srflx: Option<&str>,
    peer_srflx: &[String],
    peer_has_lan_candidate: bool,
) -> bool {
    if !peer_has_lan_candidate || peer_srflx.is_empty() {
        return false;
    }
    let Some(my_ip) = my_srflx
        .and_then(|s| s.parse::<SocketAddr>().ok())
        .map(|s| s.ip())
    else {
        return false;
    };
    peer_srflx
        .iter()
        .all(|e| e.parse::<SocketAddr>().ok().map(|s| s.ip()) == Some(my_ip))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P8 — the hairpin skip: all-peer-srflx-on-our-IP + a LAN candidate ⇒
    /// pointless; any differing IP, no LAN candidate, or unknown own srflx ⇒
    /// still worth trying.
    #[test]
    fn srflx_hairpin_pointless_cases() {
        let mine = Some("37.63.112.129:58770");
        let same = vec!["37.63.112.129:63669".to_string()];
        let mixed = vec![
            "37.63.112.129:63669".to_string(),
            "203.0.113.9:40000".to_string(),
        ];
        assert!(srflx_hairpin_pointless(mine, &same, true));
        assert!(
            !srflx_hairpin_pointless(mine, &same, false),
            "no LAN candidate ⇒ the hairpin may be all there is"
        );
        assert!(
            !srflx_hairpin_pointless(mine, &mixed, true),
            "a differing srflx IP ⇒ a real cross-NAT punch exists"
        );
        assert!(
            !srflx_hairpin_pointless(None, &same, true),
            "own srflx unknown"
        );
        assert!(
            !srflx_hairpin_pointless(mine, &[], true),
            "peer has no srflx"
        );
    }

    /// rc.210 — make-before-break is DEFAULT-ON with an explicit kill-switch.
    /// (Serialises env mutation; the overlay-l3 suite runs `--test-threads=1`.)
    #[test]
    fn make_before_break_defaults_on_with_kill_switch() {
        let n = "ROOMLER_NODE_OVERLAY_MBB";
        let a = "ROOMLER_AGENT_OVERLAY_MBB";
        let (rn, ra) = (std::env::var(n).ok(), std::env::var(a).ok());
        unsafe {
            std::env::remove_var(n);
            std::env::remove_var(a);
        }
        assert!(make_before_break_enabled(), "unset → default ON");
        for v in ["1", "true", "on", "yes", "", "  ", "anything"] {
            unsafe { std::env::set_var(n, v) };
            assert!(make_before_break_enabled(), "{v:?} → ON");
        }
        for v in ["0", "false", "FALSE", "No", "off", " off "] {
            unsafe { std::env::set_var(n, v) };
            assert!(!make_before_break_enabled(), "{v:?} → kill-switch OFF");
        }
        unsafe {
            match rn {
                Some(v) => std::env::set_var(n, v),
                None => std::env::remove_var(n),
            }
            match ra {
                Some(v) => std::env::set_var(a, v),
                None => std::env::remove_var(a),
            }
        }
    }

    /// rc.307 — stable direct-port resolution: unset → the built-in default;
    /// a number → itself; `0` → ephemeral opt-out; 65535 (would overflow the
    /// public dialer's `port+1`) and garbage → the default, NEVER silently
    /// ephemeral (a typo must not turn the feature off fleet-wide).
    #[test]
    fn direct_port_resolution() {
        let n = "ROOMLER_NODE_OVERLAY_DIRECT_PORT";
        let a = "ROOMLER_AGENT_OVERLAY_DIRECT_PORT";
        let (rn, ra) = (std::env::var(n).ok(), std::env::var(a).ok());
        unsafe {
            std::env::remove_var(n);
            std::env::remove_var(a);
        }
        assert_eq!(direct_port(), DEFAULT_DIRECT_PORT, "unset → default");
        for (v, want) in [
            ("41648", 41648u16),
            (" 12345 ", 12345),
            ("0", 0),
            ("65534", 65534),
            ("65535", DEFAULT_DIRECT_PORT),
            ("70000", DEFAULT_DIRECT_PORT),
            ("porty", DEFAULT_DIRECT_PORT),
            ("", DEFAULT_DIRECT_PORT),
        ] {
            unsafe { std::env::set_var(n, v) };
            assert_eq!(direct_port(), want, "{v:?}");
        }
        unsafe {
            match rn {
                Some(v) => std::env::set_var(n, v),
                None => std::env::remove_var(n),
            }
            match ra {
                Some(v) => std::env::set_var(a, v),
                None => std::env::remove_var(a),
            }
        }
    }

    /// rc.275 hygiene — the LAN-gather deny-list over the field-observed
    /// interface inventory: pc50045's WSL vEthernet + Check Point adapter
    /// (whose friendly name is just "Ethernet" — only the DESCRIPTION gives
    /// it away) must be denied; every real physical NIC stays allowed.
    #[test]
    fn lan_iface_denied_matrix() {
        // (name, description, denied) — descriptions verbatim from the field.
        let cases: &[(&str, &str, bool)] = &[
            // pc50045's poison trio (2026-07-30):
            (
                "vEthernet (WSL (Hyper-V firewall))",
                "Hyper-V Virtual Ethernet Adapter",
                true,
            ),
            (
                "Ethernet",
                "Check Point Virtual Network Adapter For Endpoint VPN Client",
                true,
            ),
            ("LAN-Verbindung", "Wintun Userspace Tunnel", true),
            // Real NICs across the fleet — must stay allowed:
            ("WLAN", "Intel(R) Wi-Fi 6E AX211 160MHz", false),
            ("WLAN", "Intel(R) Wi-Fi 7 BE200 320MHz", false),
            ("Ethernet", "Intel(R) Ethernet Controller I226-LM", false),
            ("Ethernet", "Realtek PCIe GbE Family Controller", false),
            ("Local Area Connection", "Broadcom NetXtreme Gigabit", false),
            ("Mobilfunk", "Intel(R)XMM(TM)7560R+ LTE-A Pro", false),
            // Unix naming conventions (no description available):
            ("eth0", "", false),
            ("enp3s0", "", false),
            ("wlp2s0", "", false),
            ("wlan0", "", false),
            ("en0", "", false),
            ("docker0", "", true),
            ("veth1a2b3c", "", true),
            ("virbr0", "", true),
            ("br-4f5e6d", "", true),
            ("bridge100", "", true),
            ("tun0", "", true),
            ("tap0", "", true),
            ("utun3", "", true),
            ("wg0", "", true),
            ("ppp0", "", true),
            ("roomler0", "", true),
            ("tailscale0", "", true),
            // TAP/OpenVPN on Windows hides behind a generic name too:
            ("Ethernet 2", "TAP-Windows Adapter V9", true),
        ];
        for (name, desc, want) in cases {
            assert_eq!(
                lan_iface_denied(name, desc),
                *want,
                "name={name:?} desc={desc:?}"
            );
        }
    }

    /// rc.275 hygiene — the filter is DEFAULT-ON with an explicit kill-switch,
    /// mirroring `direct_enabled`. (Serialises env mutation; the overlay-l3
    /// suite runs `--test-threads=1`.)
    #[test]
    fn lan_iface_filter_defaults_on_with_kill_switch() {
        let n = "ROOMLER_NODE_OVERLAY_LAN_IFACE_FILTER";
        let a = "ROOMLER_AGENT_OVERLAY_LAN_IFACE_FILTER";
        let (rn, ra) = (std::env::var(n).ok(), std::env::var(a).ok());
        unsafe {
            std::env::remove_var(n);
            std::env::remove_var(a);
        }
        assert!(lan_iface_filter_enabled(), "unset → default ON");
        for v in ["1", "true", "on", "yes", "", "  ", "anything"] {
            unsafe { std::env::set_var(n, v) };
            assert!(lan_iface_filter_enabled(), "{v:?} → ON");
        }
        for v in ["0", "false", "FALSE", "No", "off", " off "] {
            unsafe { std::env::set_var(n, v) };
            assert!(!lan_iface_filter_enabled(), "{v:?} → kill-switch OFF");
        }
        unsafe {
            match rn {
                Some(v) => std::env::set_var(n, v),
                None => std::env::remove_var(n),
            }
            match ra {
                Some(v) => std::env::set_var(a, v),
                None => std::env::remove_var(a),
            }
        }
    }

    /// rc.276 (B-probe) — forced TLS-relay is DEFAULT-OFF opt-in (positive
    /// truthy only), mirroring `public_direct_enabled`. (Serialises env
    /// mutation; the overlay-l3 suite runs `--test-threads=1`.)
    #[test]
    fn relay_tls_forced_defaults_off_with_opt_in() {
        let n = "ROOMLER_NODE_OVERLAY_RELAY_TLS";
        let a = "ROOMLER_AGENT_OVERLAY_RELAY_TLS";
        let (rn, ra) = (std::env::var(n).ok(), std::env::var(a).ok());
        unsafe {
            std::env::remove_var(n);
            std::env::remove_var(a);
        }
        assert!(!relay_tls_forced(), "unset → default OFF");
        for v in ["1", "true", "On", "yes"] {
            unsafe { std::env::set_var(n, v) };
            assert!(relay_tls_forced(), "{v:?} → opt-in ON");
        }
        for v in ["0", "false", "off", "", "  ", "anything"] {
            unsafe { std::env::set_var(n, v) };
            assert!(!relay_tls_forced(), "{v:?} → stays OFF");
        }
        unsafe {
            match rn {
                Some(v) => std::env::set_var(n, v),
                None => std::env::remove_var(n),
            }
            match ra {
                Some(v) => std::env::set_var(a, v),
                None => std::env::remove_var(a),
            }
        }
    }

    /// bind-to-interface-by-route is DEFAULT-OFF opt-in (positive truthy only),
    /// mirroring `public_direct_enabled`. (Serialises env mutation.)
    #[test]
    fn bind_by_route_defaults_off_with_opt_in() {
        let n = "ROOMLER_NODE_OVERLAY_BIND_BY_ROUTE";
        let a = "ROOMLER_AGENT_OVERLAY_BIND_BY_ROUTE";
        let (rn, ra) = (std::env::var(n).ok(), std::env::var(a).ok());
        unsafe {
            std::env::remove_var(n);
            std::env::remove_var(a);
        }
        assert!(!bind_by_route_enabled(), "unset → default OFF");
        for v in ["1", "true", "On", "yes"] {
            unsafe { std::env::set_var(n, v) };
            assert!(bind_by_route_enabled(), "{v:?} → opt-in ON");
        }
        for v in ["0", "false", "off", "", "  ", "anything"] {
            unsafe { std::env::set_var(n, v) };
            assert!(!bind_by_route_enabled(), "{v:?} → stays OFF");
        }
        unsafe {
            match rn {
                Some(v) => std::env::set_var(n, v),
                None => std::env::remove_var(n),
            }
            match ra {
                Some(v) => std::env::set_var(a, v),
                None => std::env::remove_var(a),
            }
        }
    }

    /// VPN-bypass is DEFAULT-OFF opt-in; `vpn_bypass_ifindex` resolves only when
    /// the gate is ON **and** an uplink ifindex override is set. (Serialises
    /// env mutation; the overlay-l3 suite runs `--test-threads=1`.)
    #[test]
    fn vpn_bypass_gate_and_ifindex_override() {
        let g = "ROOMLER_NODE_OVERLAY_VPN_BYPASS";
        let ga = "ROOMLER_AGENT_OVERLAY_VPN_BYPASS";
        let u = "ROOMLER_NODE_OVERLAY_UPLINK_IF";
        let ua = "ROOMLER_AGENT_OVERLAY_UPLINK_IF";
        let save = [g, ga, u, ua].map(|k| (k, std::env::var(k).ok()));
        unsafe {
            for k in [g, ga, u, ua] {
                std::env::remove_var(k);
            }
        }
        assert!(!vpn_bypass_enabled(), "unset → OFF");
        assert_eq!(uplink_ifindex_override(), None);
        assert_eq!(vpn_bypass_ifindex(), None);

        // Uplink set but gate off → no pin.
        unsafe { std::env::set_var(u, "25") };
        assert_eq!(uplink_ifindex_override(), Some(25));
        assert_eq!(
            vpn_bypass_ifindex(),
            None,
            "gate off → no pin even with uplink"
        );

        // Gate on + uplink set → pin to the uplink ifindex.
        unsafe { std::env::set_var(g, "1") };
        assert!(vpn_bypass_enabled());
        assert_eq!(vpn_bypass_ifindex(), Some(25), "gate on + uplink → pin");

        // Gate on but no/garbage uplink → None (nothing to pin to yet).
        unsafe {
            std::env::remove_var(u);
            std::env::set_var(u, "notanumber");
        }
        assert_eq!(vpn_bypass_ifindex(), None, "unparseable uplink → None");

        unsafe {
            for (k, v) in save {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    /// The pure egress classifier: the OS-chosen source IP mapped against the
    /// interfaces we hold a socket on, with the CGNAT loop-guard.
    #[test]
    fn classify_egress_cases() {
        let wifi: Ipv4Addr = "192.168.68.106".parse().unwrap();
        let eth: Ipv4Addr = "172.30.224.45".parse().unwrap();
        let ours = [wifi, eth]; // a multi-homed host's real LAN sockets
        // On-link /24 wins over the VPN default → the OS sources from the WiFi
        // NIC → Use it (the pc50045-through-VPN happy path).
        assert_eq!(classify_egress(Some(wifi), &ours), Egress::Use(wifi));
        // Multi-homed: whichever real interface the OS picks is used verbatim.
        assert_eq!(classify_egress(Some(eth), &ours), Egress::Use(eth));
        // The OS routes the dst via an interface we don't hold a LAN socket on
        // (a full-tunnel VPN captured it) → Foreign → skip direct → relay.
        let vpn: Ipv4Addr = "10.66.24.53".parse().unwrap();
        assert_eq!(classify_egress(Some(vpn), &ours), Egress::Foreign);
        // The route resolves back into the overlay's own CGNAT TUN → Loop.
        assert_eq!(
            classify_egress(Some("100.64.0.2".parse().unwrap()), &ours),
            Egress::Loop
        );
        // Query failed → Unknown → caller keeps the same-subnet behaviour.
        assert_eq!(classify_egress(None, &ours), Egress::Unknown);
        // Empty socket set (relay-only host) → any real source is Foreign.
        assert_eq!(classify_egress(Some(wifi), &[]), Egress::Foreign);
    }

    /// The connect()-trick resolves a plausible source IP for a routable dst on
    /// any platform (no packet is sent; we only read the bound local address).
    #[tokio::test]
    async fn os_src_ip_for_returns_a_source_on_a_routable_dst() {
        // 8.8.8.8 resolves via the default route on any host with networking;
        // loopback is the fallback that always works in a sandbox.
        let via_loopback = os_src_ip_for("127.0.0.1:9".parse().unwrap()).await;
        assert_eq!(
            via_loopback,
            Some(Ipv4Addr::LOCALHOST),
            "a loopback dst sources from loopback"
        );
    }

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
        let socks = vec![("127.0.0.1".parse().unwrap(), sock.clone())];
        let got = gather_srflx(&socks, pub_srv, Duration::from_millis(500)).await;
        assert_eq!(got.len(), 1, "one public srflx");
        assert_eq!(got[0].0, "203.0.113.9:40000".to_string());
        assert!(
            Arc::ptr_eq(&got[0].1, &sock),
            "candidate carries its own socket"
        );

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
            resolve_stun_server(&["stun:5.9.157.221:3478".to_string()], &[]).await,
            Some(want)
        );
        // Empty → None (srflx tier inert).
        assert_eq!(resolve_stun_server(&[], &[]).await, None);
        // A malformed leading entry (no `host:port`, so `lookup_host` errors
        // immediately without network I/O) is skipped → the usable IP literal
        // behind it wins.
        assert_eq!(
            resolve_stun_server(
                &[
                    "not-a-host-port".to_string(),
                    "stun:5.9.157.221:3478".to_string(),
                ],
                &[]
            )
            .await,
            Some(want)
        );
        // A worker CO-LOCATED with this host (its IP is in `exclude`) is skipped
        // → the next worker wins. Prevents a fleet host from STUNning itself.
        assert_eq!(
            resolve_stun_server(
                &[
                    "stun:94.130.141.74:3478".to_string(),
                    "stun:5.9.157.221:3478".to_string(),
                ],
                &["94.130.141.74".parse().unwrap()],
            )
            .await,
            Some(want)
        );
        // ALL targets co-located → None (correctly: this host truly can't STUN a
        // NON-self worker, so it has no usable srflx from this set).
        assert_eq!(
            resolve_stun_server(
                &["stun:94.130.141.74:3478".to_string()],
                &["94.130.141.74".parse().unwrap()],
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn resolve_stun_targets_prefers_distinct_ips_else_ports() {
        // Two IP-literal URLs on distinct IPs → both returned (strongest probe).
        let t = resolve_stun_targets(
            &[
                "stun:5.9.157.221:3478".to_string(),
                "stun:94.130.141.98:3478".to_string(),
            ],
            &[],
        )
        .await;
        assert_eq!(t.len(), 2);
        assert_ne!(t[0].ip(), t[1].ip());
        // Same IP, two ports → distinct-port fallback (still two targets).
        let t2 = resolve_stun_targets(
            &[
                "stun:5.9.157.221:3478".to_string(),
                "stun:5.9.157.221:443".to_string(),
            ],
            &[],
        )
        .await;
        assert_eq!(t2.len(), 2);
        assert_eq!(t2[0].ip(), t2[1].ip());
        assert_ne!(t2[0].port(), t2[1].port());
        // A single target → len 1 (the caller can't classify).
        assert_eq!(
            resolve_stun_targets(&["stun:5.9.157.221:3478".to_string()], &[])
                .await
                .len(),
            1
        );
        // Empty → empty.
        assert!(resolve_stun_targets(&[], &[]).await.is_empty());
        // A co-located worker is excluded → only the non-self target remains.
        let t3 = resolve_stun_targets(
            &[
                "stun:94.130.141.74:3478".to_string(),
                "stun:5.9.157.221:3478".to_string(),
            ],
            &["94.130.141.74".parse().unwrap()],
        )
        .await;
        assert_eq!(t3.len(), 1);
        assert_eq!(t3[0].ip().to_string(), "5.9.157.221");
    }

    #[tokio::test]
    async fn probe_nat_type_cone_vs_symmetric_else_none() {
        // Cone: both servers observe the SAME public mapping.
        let (a, _h1) = fake_stun_server([203, 0, 113, 9], 5000).await;
        let (b, _h2) = fake_stun_server([203, 0, 113, 9], 5000).await;
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        assert_eq!(
            probe_nat_type(&sock, &[a, b], Duration::from_millis(500)).await,
            Some("cone")
        );

        // Symmetric: the two servers observe DIFFERENT ports.
        let (c, _h3) = fake_stun_server([203, 0, 113, 9], 5000).await;
        let (d, _h4) = fake_stun_server([203, 0, 113, 9], 6000).await;
        let sock2 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        assert_eq!(
            probe_nat_type(&sock2, &[c, d], Duration::from_millis(500)).await,
            Some("symmetric")
        );

        // Fewer than two targets → can't classify.
        let sock3 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        assert_eq!(
            probe_nat_type(&sock3, &[a], Duration::from_millis(200)).await,
            None
        );
        // A dead target → None (caller stays optimistic and attempts anyway).
        let dead: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let sock4 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        assert_eq!(
            probe_nat_type(&sock4, &[a, dead], Duration::from_millis(150)).await,
            None
        );
    }

    #[test]
    fn srflx_punch_worth_trying_skips_only_both_symmetric() {
        // Skip ONLY when both ends are confidently symmetric.
        assert!(!srflx_punch_worth_trying(
            Some("symmetric"),
            Some("symmetric")
        ));
        // Any cone / unknown side → attempt.
        assert!(srflx_punch_worth_trying(Some("symmetric"), Some("cone")));
        assert!(srflx_punch_worth_trying(Some("cone"), Some("symmetric")));
        assert!(srflx_punch_worth_trying(Some("cone"), Some("cone")));
        assert!(srflx_punch_worth_trying(Some("symmetric"), None));
        assert!(srflx_punch_worth_trying(None, Some("symmetric")));
        assert!(srflx_punch_worth_trying(None, None));
    }
}

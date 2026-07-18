//! L3 TUN surface for the overlay (Phase 3).
//!
//! [`TunIo`] is the seam between the WireGuard bridge ([`super::bridge`])
//! and the OS virtual NIC. Production uses [`SystemTun`] (the `tun` crate
//! → Wintun on Windows, `/dev/net/tun` on Linux, utun on macOS), behind
//! the `overlay-l3` feature; tests use an in-memory mock, so the bridge
//! is exercised end-to-end with no kernel driver and no privilege.
//!
//! Routing note: a node brings the device up with its own overlay IP and
//! the *network* netmask (e.g. `100.64.0.3` / `255.192.0.0` for a `/10`),
//! which makes the whole overlay CIDR on-link via this interface — the OS
//! installs the connected route automatically on Linux + Windows, so
//! there is no explicit route-table call here. Per-peer reachability is
//! still exact-match `/32` in [`super::router::Router`]; a packet to an
//! overlay address with no installed peer is dropped by
//! [`super::wg::WgDevice::send_ip_packet`]. (macOS utun is point-to-point
//! and may need an explicit `route add` for the CIDR — refined when 3b/3c
//! field-test there.)
//!
//! Dual-stack: the device also carries the node's *derived* overlay IPv6
//! ([`super::router::derive_overlay_v6`]) on the ULA `/96`, assigned
//! best-effort at bring-up — the connected `/96` route makes every peer's
//! derived v6 on-link, and the WG bridge routes those packets by unmapping
//! the ULA destination to its embedded v4 (no v6 route table anywhere).

use async_trait::async_trait;

/// One IP packet in / one IP packet out. Implemented by [`SystemTun`]
/// (real device, `overlay-l3`) and, in tests, an in-memory mock — so
/// [`super::bridge::run_bridge`] is agnostic to the underlying NIC.
#[async_trait]
pub trait TunIo: Send + Sync {
    /// Read the next IP packet from the device. Blocks until one is
    /// available; `Err` means the device is gone and the bridge's
    /// outbound loop should exit.
    async fn read_packet(&self) -> std::io::Result<Vec<u8>>;

    /// Write one IP packet to the device.
    async fn write_packet(&self, packet: &[u8]) -> std::io::Result<()>;

    /// Install a host (`/32`) route for a peer's overlay IP via this device,
    /// so overlay traffic out-specifics any colliding *less*-specific route on
    /// the host's uplink — e.g. an ISP/corp **CGNAT `100.64.0.0/10`** that
    /// otherwise swallows the packets. The connected-CIDR route alone is not
    /// enough on such a host (field bug 2026-06-10: PC50045's pings to peers
    /// leaked to its carrier's CGNAT until a manual `/32` was added). Default
    /// no-op (the in-memory mock + platforms where the connected route is
    /// sufficient). **Best-effort:** a failure is logged by the caller, not
    /// fatal — direct/clean hosts route fine via the `/10` regardless.
    async fn add_peer_route(&self, _peer: std::net::Ipv4Addr) -> std::io::Result<()> {
        Ok(())
    }

    /// Remove the `/32` installed by [`add_peer_route`] (the peer left the
    /// mesh). Best-effort; never fails the caller.
    async fn del_peer_route(&self, _peer: std::net::Ipv4Addr) {}

    /// Phase 1 — install an OS route for a subnet `cidr` (e.g. `"192.168.1.0/24"`)
    /// via this device, so LAN behind a router-peer is reachable over the
    /// overlay. Default no-op; best-effort.
    async fn add_cidr_route(&self, _cidr: &str) -> std::io::Result<()> {
        Ok(())
    }

    /// Remove a CIDR route installed by [`add_cidr_route`]. Best-effort.
    async fn del_cidr_route(&self, _cidr: &str) {}

    /// P5 exit-node — install a `/32` (host) **exemption** route for `ip` via the
    /// host's ORIGINAL default gateway (captured at TUN bring-up), NOT via this
    /// overlay device. When an exit-node client installs the split-default
    /// (`0.0.0.0/1` + `128.0.0.0/1`) via the overlay, these longer-prefix `/32`s
    /// keep the carrier-critical endpoints — the coordination server, the coturn
    /// relay, and the exit peer's own WG endpoint — flowing over the real uplink,
    /// so the default capture can never sever the very tunnel that carries it.
    /// Default no-op (the in-memory mock, netstack, or when no default route was
    /// discovered); best-effort — a failure is surfaced by the split-tunnel check.
    async fn add_host_exemption(&self, _ip: std::net::IpAddr) -> std::io::Result<()> {
        Ok(())
    }

    /// Remove a `/32` exemption installed by [`add_host_exemption`]. Best-effort.
    async fn del_host_exemption(&self, _ip: std::net::IpAddr) {}
}

/// The real OS TUN device. Behind `overlay-l3` so the WG core + the
/// bridge logic stay device-free (and dependency-free) under plain
/// `overlay`.
#[cfg(feature = "overlay-l3")]
pub use system::{SystemTun, purge_split_default};

#[cfg(feature = "overlay-l3")]
mod system {
    use std::net::Ipv4Addr;
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::TunIo;

    /// A live OS TUN device. `tun::AsyncDevice::{recv,send}` take `&self`,
    /// so a single `Arc<AsyncDevice>` backs the bridge's concurrent read
    /// + write loops.
    pub struct SystemTun {
        dev: Arc<tun::AsyncDevice>,
        /// P5 — the host's ORIGINAL default route (gateway + interface), captured
        /// at bring-up BEFORE any overlay route can shadow it. Used to pin
        /// exit-node carrier-endpoint exemption `/32`s via the real uplink (see
        /// [`TunIo::add_host_exemption`]). `None` when discovery failed — the
        /// split-tunnel check (S4) then surfaces a WARN rather than wedging.
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        orig_default: Option<OrigDefaultRoute>,
        /// WFP hard-permit guard. Holds a dynamic WFP session whose `Drop`
        /// reaps the `roomler`-adapter permit filters, so it must live as
        /// long as the device. `None` when disabled
        /// (`ROOMLER_AGENT_WFP_PERMIT=0`) or when install failed
        /// (best-effort — the overlay still works on non-locked hosts).
        #[cfg(windows)]
        _wfp: Option<crate::overlay::wfp::WfpGuard>,
    }

    impl SystemTun {
        /// Create the device, assign `self_ip` with `netmask`, set `mtu`,
        /// and bring it up. `netmask` is the overlay *network* mask (e.g.
        /// `/10` → `255.192.0.0`) so the whole overlay CIDR routes here
        /// via the OS-installed connected route. Must be called inside a
        /// Tokio runtime (the async device registers with the reactor).
        ///
        /// Dual-stack: the device also gets this node's *derived* overlay
        /// IPv6 ([`derive_overlay_v6`](crate::overlay::router::derive_overlay_v6))
        /// on the ULA `/96` (best-effort, on
        /// Linux and Windows) — the OS-TUN mirror of the netstack's
        /// dual-addressed iface. The connected `/96` route auto-installs,
        /// making every peer's derived v6 on-link; the WG bridge routes it
        /// by unmapping the ULA destination to its embedded v4
        /// ([`Router::dst_of_ip_packet`](crate::overlay::router::Router::dst_of_ip_packet)).
        /// No per-peer v6 `/128`s and no v6 metric pin: unlike the CGNAT
        /// `100.64.0.0/10`, nothing else on a host claims our random ULA, so
        /// there is no route war to win (the reason the v4 side needs both).
        pub fn up(self_ip: Ipv4Addr, netmask: Ipv4Addr, mtu: u16) -> std::io::Result<Self> {
            let mut config = tun::Configuration::default();
            config.address(self_ip).netmask(netmask).mtu(mtu).up();

            // Stable adapter name so a reconnect reuses the same NIC
            // (Wintun keys adapters by name) instead of accreting copies.
            #[cfg(target_os = "windows")]
            config.tun_name("roomler");
            #[cfg(target_os = "linux")]
            config.tun_name("roomler0");

            let dev =
                tun::create_as_async(&config).map_err(|e| std::io::Error::other(e.to_string()))?;
            let dev = Arc::new(dev);

            // Pin the overlay NIC to the lowest interface metric so its routes
            // (the connected `/10` + the per-peer `/32`s) are preferred over a
            // full-tunnel VPN's captured routes for the overlay's 100.64.0.0/10
            // range. Check Point Endpoint installs competing `/32`s via its NIC
            // at metric 1, which otherwise swallow overlay traffic (the packet
            // is bounced to the VPN gateway → "destination host unreachable").
            // Best-effort + sync (`up` isn't async) — a blocking `netsh` at
            // bring-up is fine; a failure just leaves the default metric.
            #[cfg(target_os = "windows")]
            {
                let _ = std::process::Command::new("netsh")
                    .args(["interface", "ipv4", "set", "interface", IF_NAME, "metric=1"])
                    .output();
            }

            // Dual-stack: assign the derived overlay v6 on the ULA /96 (the
            // `tun` crate's Configuration is v4-only, so this is an OS call —
            // sync + best-effort like the metric pin; a failure leaves the
            // node v4-only, which keeps working unchanged).
            assign_derived_v6(self_ip);

            // Program WFP so the overlay's inbound survives a GPO-locked
            // Defender Firewall (Tailscale's approach). Best-effort: a
            // failure is logged and the overlay still comes up — it only
            // matters on hosts where the firewall is the blocker.
            #[cfg(windows)]
            let _wfp = if crate::overlay::wfp::wfp_enabled() {
                use tun::AbstractDeviceExt as _;
                let luid = dev.tun_luid();
                match crate::overlay::wfp::WfpGuard::install(luid) {
                    Ok(g) => {
                        tracing::info!(
                            luid = format_args!("{luid:#x}"),
                            "overlay: WFP hard-permit installed for the roomler adapter"
                        );
                        Some(g)
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "overlay: WFP permit NOT installed; if inbound traffic fails behind a \
                             GPO-locked firewall, request an IT-managed exception for the roomler adapter"
                        );
                        None
                    }
                }
            } else {
                tracing::info!("overlay: WFP permit disabled via ROOMLER_AGENT_WFP_PERMIT");
                None
            };

            // P5 — snapshot the host's original default route NOW, before any
            // overlay route is installed, so exit-node exemptions can later pin
            // carrier-critical endpoints via the real uplink.
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            let orig_default = {
                let d = discover_default_route();
                match &d {
                    Some(r) => tracing::info!(
                        gateway = %r.gateway, interface = %r.interface,
                        "overlay: captured original default route (exit-node exemptions available)"
                    ),
                    None => tracing::warn!(
                        "overlay: no original default route found; \
                         exit-node carrier exemptions will be unavailable"
                    ),
                }
                d
            };

            Ok(Self {
                dev,
                #[cfg(any(target_os = "linux", target_os = "windows"))]
                orig_default,
                #[cfg(windows)]
                _wfp,
            })
        }
    }

    #[async_trait]
    impl TunIo for SystemTun {
        async fn read_packet(&self) -> std::io::Result<Vec<u8>> {
            // Overlay MTU is 1280; 1600 covers it plus any platform
            // packet-information headroom the crate may surface.
            let mut buf = vec![0u8; 1600];
            let n = self
                .dev
                .recv(&mut buf)
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            buf.truncate(n);
            Ok(buf)
        }

        async fn write_packet(&self, packet: &[u8]) -> std::io::Result<()> {
            self.dev
                .send(packet)
                .await
                .map(|_| ())
                .map_err(|e| std::io::Error::other(e.to_string()))
        }

        /// Add an on-link `/32` for `peer` via the overlay NIC. Windows uses
        /// `netsh` (by adapter name, so no LUID/index lookup); Linux uses
        /// `ip route replace` (idempotent). macOS utun is left to the
        /// connected route for now (refined when 3b/3c field-test there). The
        /// agent runs privileged (service), so the route call has rights.
        async fn add_peer_route(&self, peer: Ipv4Addr) -> std::io::Result<()> {
            #[cfg(target_os = "windows")]
            {
                // A full-tunnel VPN (Check Point Endpoint) installs a competing
                // `/32` for each overlay peer via its own NIC at metric 1, which
                // swallows overlay traffic. The overlay OWNS 100.64.0.0/10, so
                // any non-wintun route for a peer is wrong by construction:
                // evict ALL `/32`s for this IP (cross-interface `route delete`),
                // then (re-)add ours via the wintun at a low metric so it wins
                // even if the VPN re-adds later. `route delete` erroring (no
                // such route on first install) is expected → ignored.
                let _ = run_cmd(
                    "route",
                    vec![
                        "delete".into(),
                        peer.to_string(),
                        "mask".into(),
                        "255.255.255.255".into(),
                    ],
                )
                .await;
                run_cmd(
                    "netsh",
                    vec![
                        "interface".into(),
                        "ipv4".into(),
                        "add".into(),
                        "route".into(),
                        format!("prefix={peer}/32"),
                        format!("interface={IF_NAME}"),
                        "metric=1".into(),
                        "store=active".into(),
                    ],
                )
                .await
            }
            #[cfg(target_os = "linux")]
            {
                run_cmd(
                    "ip",
                    vec![
                        "route".into(),
                        "replace".into(),
                        format!("{peer}/32"),
                        "dev".into(),
                        IF_NAME.into(),
                    ],
                )
                .await
            }
            #[cfg(not(any(target_os = "windows", target_os = "linux")))]
            {
                let _ = peer;
                Ok(())
            }
        }

        async fn del_peer_route(&self, peer: Ipv4Addr) {
            #[cfg(target_os = "windows")]
            let _ = run_cmd(
                "netsh",
                vec![
                    "interface".into(),
                    "ipv4".into(),
                    "delete".into(),
                    "route".into(),
                    format!("prefix={peer}/32"),
                    format!("interface={IF_NAME}"),
                ],
            )
            .await;
            #[cfg(target_os = "linux")]
            let _ = run_cmd(
                "ip",
                vec![
                    "route".into(),
                    "del".into(),
                    format!("{peer}/32"),
                    "dev".into(),
                    IF_NAME.into(),
                ],
            )
            .await;
            #[cfg(not(any(target_os = "windows", target_os = "linux")))]
            let _ = peer;
        }

        /// Phase 1 — install an OS route for `cidr` via the overlay NIC (a
        /// subnet a router-peer serves). Idempotent (delete-then-add on Windows;
        /// `ip route replace` on Linux). Low metric so it wins a colliding uplink
        /// route, mirroring the per-peer `/32` path.
        ///
        /// Dual-stack (P5): `cidr` may be IPv4 or IPv6 — the family is picked from
        /// the string. Exit-node routing uses this for BOTH the v4 split-default
        /// (`0.0.0.0/1` + `128.0.0.0/1`, which encapsulate to the exit peer) AND
        /// the v6 fail-closed halves (`::/1` + `8000::/1`, which the crypto-router
        /// drops because global v6 is unroutable over the overlay — so v6 can't
        /// leak out the physical uplink while v4 egress is captured).
        async fn add_cidr_route(&self, cidr: &str) -> std::io::Result<()> {
            let v6 = is_v6_cidr(cidr);
            #[cfg(target_os = "windows")]
            {
                let family = if v6 { "ipv6" } else { "ipv4" };
                let _ = run_cmd(
                    "netsh",
                    vec![
                        "interface".into(),
                        family.into(),
                        "delete".into(),
                        "route".into(),
                        format!("prefix={cidr}"),
                        format!("interface={IF_NAME}"),
                    ],
                )
                .await;
                run_cmd(
                    "netsh",
                    vec![
                        "interface".into(),
                        family.into(),
                        "add".into(),
                        "route".into(),
                        format!("prefix={cidr}"),
                        format!("interface={IF_NAME}"),
                        "metric=1".into(),
                        "store=active".into(),
                    ],
                )
                .await
            }
            #[cfg(target_os = "linux")]
            {
                let mut args: Vec<String> = Vec::new();
                if v6 {
                    args.push("-6".into());
                }
                args.extend([
                    "route".into(),
                    "replace".into(),
                    cidr.to_string(),
                    "dev".into(),
                    IF_NAME.into(),
                ]);
                run_cmd("ip", args).await
            }
            #[cfg(not(any(target_os = "windows", target_os = "linux")))]
            {
                let _ = (cidr, v6);
                Ok(())
            }
        }

        async fn del_cidr_route(&self, cidr: &str) {
            let v6 = is_v6_cidr(cidr);
            #[cfg(target_os = "windows")]
            let _ = run_cmd(
                "netsh",
                vec![
                    "interface".into(),
                    if v6 { "ipv6" } else { "ipv4" }.into(),
                    "delete".into(),
                    "route".into(),
                    format!("prefix={cidr}"),
                    format!("interface={IF_NAME}"),
                ],
            )
            .await;
            #[cfg(target_os = "linux")]
            {
                let mut args: Vec<String> = Vec::new();
                if v6 {
                    args.push("-6".into());
                }
                args.extend([
                    "route".into(),
                    "del".into(),
                    cidr.to_string(),
                    "dev".into(),
                    IF_NAME.into(),
                ]);
                let _ = run_cmd("ip", args).await;
            }
            #[cfg(not(any(target_os = "windows", target_os = "linux")))]
            let _ = (cidr, v6);
        }

        /// P5 — install a `/32` exemption for `ip` via the host's ORIGINAL default
        /// gateway (captured at bring-up), NOT the overlay NIC, so the exit-node
        /// split-default can't capture this carrier-critical endpoint. `Err` when
        /// no default route was discovered (the caller's split-tunnel check
        /// surfaces it). IPv6 exemptions are deferred to the v6-egress slice.
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        async fn add_host_exemption(&self, ip: std::net::IpAddr) -> std::io::Result<()> {
            let std::net::IpAddr::V4(v4) = ip else {
                return Ok(()); // v6 handled by the v6-egress slice (S3b)
            };
            let Some(gw) = self.orig_default.as_ref() else {
                return Err(std::io::Error::other(
                    "no original default route captured; cannot exempt carrier endpoint",
                ));
            };
            #[cfg(target_os = "linux")]
            {
                run_cmd(
                    "ip",
                    vec![
                        "route".into(),
                        "replace".into(),
                        format!("{v4}/32"),
                        "via".into(),
                        gw.gateway.to_string(),
                        "dev".into(),
                        gw.interface.clone(),
                    ],
                )
                .await
            }
            #[cfg(target_os = "windows")]
            {
                // delete-then-add (idempotent), matching the other route helpers.
                let _ = run_cmd(
                    "netsh",
                    vec![
                        "interface".into(),
                        "ipv4".into(),
                        "delete".into(),
                        "route".into(),
                        format!("prefix={v4}/32"),
                        format!("interface={}", gw.interface),
                    ],
                )
                .await;
                run_cmd(
                    "netsh",
                    vec![
                        "interface".into(),
                        "ipv4".into(),
                        "add".into(),
                        "route".into(),
                        format!("prefix={v4}/32"),
                        format!("interface={}", gw.interface),
                        format!("nexthop={}", gw.gateway),
                        "metric=1".into(),
                        "store=active".into(),
                    ],
                )
                .await
            }
        }

        /// Remove a `/32` exemption installed by [`Self::add_host_exemption`].
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        async fn del_host_exemption(&self, ip: std::net::IpAddr) {
            let std::net::IpAddr::V4(v4) = ip else {
                return;
            };
            let Some(gw) = self.orig_default.as_ref() else {
                return;
            };
            #[cfg(target_os = "linux")]
            let _ = run_cmd(
                "ip",
                vec![
                    "route".into(),
                    "del".into(),
                    format!("{v4}/32"),
                    "via".into(),
                    gw.gateway.to_string(),
                    "dev".into(),
                    gw.interface.clone(),
                ],
            )
            .await;
            #[cfg(target_os = "windows")]
            let _ = run_cmd(
                "netsh",
                vec![
                    "interface".into(),
                    "ipv4".into(),
                    "delete".into(),
                    "route".into(),
                    format!("prefix={v4}/32"),
                    format!("interface={}", gw.interface),
                ],
            )
            .await;
        }
    }

    /// The overlay NIC name we set in [`SystemTun::up`] — used to target
    /// per-peer `/32` routes without a LUID/index lookup.
    #[cfg(target_os = "windows")]
    const IF_NAME: &str = "roomler";
    #[cfg(target_os = "linux")]
    const IF_NAME: &str = "roomler0";

    /// Is `cidr` an IPv6 CIDR? A colon only ever appears in the v6 textual form
    /// (`"::/1"`, `"8000::/1"`, `"fd72:6f6f:6d6c::/96"`), never in a v4 one
    /// (`"0.0.0.0/1"`) — so this cheap check picks the right OS route family for
    /// [`TunIo::add_cidr_route`] / [`TunIo::del_cidr_route`] without pulling in a
    /// parser. Pure, so it unit-tests directly.
    fn is_v6_cidr(cidr: &str) -> bool {
        cidr.contains(':')
    }

    /// P5 exit-node crash-safety (A2) — synchronously delete the split-default
    /// routes (the v4 + v6 `/1` halves) from the overlay NIC WITHOUT a live
    /// [`SystemTun`]. Removes EXACTLY the
    /// [`SPLIT_DEFAULT_V4`](crate::overlay::runtime::SPLIT_DEFAULT_V4) and
    /// [`SPLIT_DEFAULT_V6`](crate::overlay::runtime::SPLIT_DEFAULT_V6) the
    /// installer installs (one source of truth), scoped to the roomler NIC so it
    /// never touches another VPN's `/1`.
    ///
    /// Two callers, both bypassing the runtime's RAII teardown:
    ///
    /// - the **boot-time reconciler** — heals a `/1` a crash / kill / unclean
    ///   reboot left behind. Critical on Windows: Wintun's adapter persists by
    ///   name across a crash, so a stale `0.0.0.0/1 interface=roomler` blackholes
    ///   ALL egress to a dead NIC until the next clean run. (On Linux a
    ///   `dev`-scoped route auto-culls when the TUN dies, but a kill mid-reroute
    ///   can still leave one, so we heal there too.)
    /// - the **pre-`process::exit` cleanup** on the paths that skip `Drop`
    ///   (watchdog stall, self-update, agent-deleted).
    ///
    /// Best-effort (an absent route just errors, ignored); sync `std::process` so
    /// it runs at boot and as a last gasp with no async runtime.
    pub fn purge_split_default() {
        for cidr in crate::overlay::runtime::SPLIT_DEFAULT_V4
            .iter()
            .chain(crate::overlay::runtime::SPLIT_DEFAULT_V6.iter())
        {
            purge_one(cidr);
        }
    }

    #[cfg(target_os = "linux")]
    fn purge_one(cidr: &str) {
        let mut args: Vec<&str> = Vec::new();
        if is_v6_cidr(cidr) {
            args.push("-6");
        }
        args.extend(["route", "del", cidr, "dev", IF_NAME]);
        let _ = std::process::Command::new("ip").args(&args).output();
    }

    #[cfg(target_os = "windows")]
    fn purge_one(cidr: &str) {
        let family = if is_v6_cidr(cidr) { "ipv6" } else { "ipv4" };
        let _ = std::process::Command::new("netsh")
            .args([
                "interface",
                family,
                "delete",
                "route",
                &format!("prefix={cidr}"),
                &format!("interface={IF_NAME}"),
            ])
            .output();
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    fn purge_one(_cidr: &str) {}

    /// Assign this node's derived overlay IPv6 (`fd72:6f6f:6d6c::<v4>`, `/96`
    /// on-link) to the overlay NIC. Sync + best-effort (`up` isn't async): the
    /// `tun` crate's `Configuration` carries no v6 surface, so Linux uses
    /// `ip -6 addr replace` (idempotent) and Windows the delete-then-add
    /// `netsh` pattern the route helpers already use (the Wintun adapter
    /// persists across reconnects, so the address may already be present).
    /// macOS utun stays v4-only for now, matching the per-peer-route stance.
    fn assign_derived_v6(self_ip: Ipv4Addr) {
        let v6 = crate::overlay::router::derive_overlay_v6(self_ip);
        #[cfg(target_os = "linux")]
        {
            let cidr = format!("{v6}/{}", crate::overlay::router::OVERLAY_V6_ONLINK_PREFIX);
            match std::process::Command::new("ip")
                .args(["-6", "addr", "replace", &cidr, "dev", IF_NAME])
                .output()
            {
                Ok(out) if out.status.success() => {
                    tracing::info!(addr = %cidr, "overlay: derived IPv6 assigned to the TUN");
                }
                Ok(out) => tracing::warn!(
                    addr = %cidr,
                    stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                    "overlay: derived-IPv6 assign failed; node stays v4-only"
                ),
                Err(e) => tracing::warn!(
                    addr = %cidr,
                    error = %e,
                    "overlay: derived-IPv6 assign failed; node stays v4-only"
                ),
            }
        }
        #[cfg(target_os = "windows")]
        {
            // Delete (ignored when absent — first bring-up), then add.
            let iface = format!("interface={IF_NAME}");
            let _ = std::process::Command::new("netsh")
                .args([
                    "interface",
                    "ipv6",
                    "delete",
                    "address",
                    &iface,
                    &format!("address={v6}"),
                ])
                .output();
            let addr = format!(
                "address={v6}/{}",
                crate::overlay::router::OVERLAY_V6_ONLINK_PREFIX
            );
            match std::process::Command::new("netsh")
                .args([
                    "interface",
                    "ipv6",
                    "add",
                    "address",
                    &iface,
                    &addr,
                    "store=active",
                ])
                .output()
            {
                Ok(out) if out.status.success() => {
                    tracing::info!(%addr, "overlay: derived IPv6 assigned to the TUN");
                }
                Ok(out) => tracing::warn!(
                    %addr,
                    stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                    "overlay: derived-IPv6 assign failed; node stays v4-only"
                ),
                Err(e) => tracing::warn!(
                    %addr,
                    error = %e,
                    "overlay: derived-IPv6 assign failed; node stays v4-only"
                ),
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            // macOS utun: v4-only for now (see the doc comment).
            let _ = v6;
        }
    }

    /// Run an OS route command off the async reactor (`std::process` in a
    /// blocking task — avoids pulling in tokio's `process` feature). Non-zero
    /// exit → `Err` with the captured stderr.
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    async fn run_cmd(prog: &'static str, args: Vec<String>) -> std::io::Result<()> {
        tokio::task::spawn_blocking(move || {
            let out = std::process::Command::new(prog).args(&args).output()?;
            if out.status.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(format!(
                    "{prog} {args:?} exited {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                )))
            }
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?
    }

    /// The host's original default route — the gateway + interface that carried
    /// its traffic BEFORE the overlay installed any route. Captured once in
    /// [`SystemTun::up`]; used to pin exit-node exemption `/32`s via the real
    /// uplink. `interface` is the Linux `dev` name / the Windows interface index.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[derive(Debug, Clone)]
    struct OrigDefaultRoute {
        gateway: Ipv4Addr,
        interface: String,
    }

    /// Query the OS for the active IPv4 default route, picking the lowest-metric
    /// one on a multi-homed host. `None` on any error or when there is none.
    #[cfg(target_os = "linux")]
    fn discover_default_route() -> Option<OrigDefaultRoute> {
        let out = std::process::Command::new("ip")
            .args(["-4", "route", "show", "default"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        parse_linux_default_route(&String::from_utf8_lossy(&out.stdout))
    }

    #[cfg(target_os = "windows")]
    fn discover_default_route() -> Option<OrigDefaultRoute> {
        let out = std::process::Command::new("netsh")
            .args(["interface", "ipv4", "show", "route"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        parse_windows_default_route(&String::from_utf8_lossy(&out.stdout))
    }

    /// Parse `ip -4 route show default` → the lowest-metric default route. Pure
    /// (OS-call-free) so it unit-tests against captured output. A default via our
    /// own overlay NIC is ignored (never exempt via ourselves).
    #[cfg(target_os = "linux")]
    fn parse_linux_default_route(output: &str) -> Option<OrigDefaultRoute> {
        fn tok_after<'a>(toks: &[&'a str], key: &str) -> Option<&'a str> {
            toks.iter()
                .position(|t| *t == key)
                .and_then(|i| toks.get(i + 1).copied())
        }
        let mut best: Option<(u32, OrigDefaultRoute)> = None;
        for line in output.lines() {
            let line = line.trim();
            if !line.starts_with("default") {
                continue;
            }
            let toks: Vec<&str> = line.split_whitespace().collect();
            let gateway = tok_after(&toks, "via").and_then(|s| s.parse::<Ipv4Addr>().ok());
            let interface = tok_after(&toks, "dev").map(str::to_string);
            let (Some(gateway), Some(interface)) = (gateway, interface) else {
                continue;
            };
            if interface == IF_NAME {
                continue; // never exempt via the overlay itself
            }
            let metric = tok_after(&toks, "metric")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0);
            if best.as_ref().is_none_or(|(m, _)| metric < *m) {
                best = Some((metric, OrigDefaultRoute { gateway, interface }));
            }
        }
        best.map(|(_, r)| r)
    }

    /// Parse `netsh interface ipv4 show route` → the lowest-metric `0.0.0.0/0`
    /// route (gateway + interface index). Pure so it unit-tests against captured
    /// output. Rows whose gateway column is an interface NAME (on-link, no
    /// gateway) are skipped — a real default route always carries a gateway.
    #[cfg(target_os = "windows")]
    fn parse_windows_default_route(output: &str) -> Option<OrigDefaultRoute> {
        let mut best: Option<(u32, OrigDefaultRoute)> = None;
        for line in output.lines() {
            // Columns: Publish  Type  Met  Prefix  Idx  Gateway/Interface Name
            let toks: Vec<&str> = line.split_whitespace().collect();
            if toks.len() < 6 || toks[3] != "0.0.0.0/0" {
                continue;
            }
            let Ok(metric) = toks[2].parse::<u32>() else {
                continue; // header row ("Met")
            };
            let Ok(gateway) = toks[5].parse::<Ipv4Addr>() else {
                continue; // on-link (interface name, not a gateway)
            };
            let interface = toks[4].to_string();
            if best.as_ref().is_none_or(|(m, _)| metric < *m) {
                best = Some((metric, OrigDefaultRoute { gateway, interface }));
            }
        }
        best.map(|(_, r)| r)
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn v6_cidr_detection_picks_route_family() {
            use super::is_v6_cidr;
            // v6 exit-node fail-closed halves + the derived-ULA prefix.
            assert!(is_v6_cidr("::/1"));
            assert!(is_v6_cidr("8000::/1"));
            assert!(is_v6_cidr("fd72:6f6f:6d6c::/96"));
            // v4 split-default halves + a normal subnet route.
            assert!(!is_v6_cidr("0.0.0.0/1"));
            assert!(!is_v6_cidr("128.0.0.0/1"));
            assert!(!is_v6_cidr("192.168.1.0/24"));
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn linux_default_route_lowest_metric_skips_overlay() {
            use super::parse_linux_default_route;
            use std::net::Ipv4Addr;
            // Lowest metric wins on a multi-homed host.
            let out = "default via 192.168.1.1 dev eth0 proto dhcp metric 100\n\
                       default via 10.8.0.1 dev tun0 metric 50\n";
            let r = parse_linux_default_route(out).unwrap();
            assert_eq!(r.gateway, Ipv4Addr::new(10, 8, 0, 1));
            assert_eq!(r.interface, "tun0");
            // Missing metric == 0 == wins.
            let r2 = parse_linux_default_route("default via 192.168.1.1 dev eth0\n").unwrap();
            assert_eq!(r2.gateway, Ipv4Addr::new(192, 168, 1, 1));
            // A default via our own overlay NIC is ignored.
            let out3 = "default via 100.64.0.1 dev roomler0 metric 1\n\
                        default via 192.168.1.1 dev eth0 metric 100\n";
            assert_eq!(parse_linux_default_route(out3).unwrap().interface, "eth0");
            // No default route present.
            assert!(parse_linux_default_route("").is_none());
            assert!(parse_linux_default_route("10.0.0.0/8 dev eth0\n").is_none());
        }

        #[cfg(target_os = "windows")]
        #[test]
        fn windows_default_route_lowest_metric_skips_headers_and_onlink() {
            use super::parse_windows_default_route;
            use std::net::Ipv4Addr;
            let out = "\
Publish  Type      Met  Prefix                    Idx  Gateway/Interface Name\r\n\
-------  --------  ---  ------------------------  ---  ------------------------\r\n\
No       Manual      0  0.0.0.0/0                   12  192.168.68.1\r\n\
No       Manual    256  0.0.0.0/0                    5  10.0.0.1\r\n\
No       Manual    256  255.255.255.255/32          1  Loopback Pseudo-Interface 1\r\n";
            let r = parse_windows_default_route(out).unwrap();
            assert_eq!(r.gateway, Ipv4Addr::new(192, 168, 68, 1));
            assert_eq!(r.interface, "12");
            // No default route present.
            assert!(parse_windows_default_route("").is_none());
        }
    }
}

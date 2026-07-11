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
}

/// The real OS TUN device. Behind `overlay-l3` so the WG core + the
/// bridge logic stay device-free (and dependency-free) under plain
/// `overlay`.
#[cfg(feature = "overlay-l3")]
pub use system::SystemTun;

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

            Ok(Self {
                dev,
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
        async fn add_cidr_route(&self, cidr: &str) -> std::io::Result<()> {
            #[cfg(target_os = "windows")]
            {
                let _ = run_cmd(
                    "netsh",
                    vec![
                        "interface".into(),
                        "ipv4".into(),
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
                        "ipv4".into(),
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
                run_cmd(
                    "ip",
                    vec![
                        "route".into(),
                        "replace".into(),
                        cidr.to_string(),
                        "dev".into(),
                        IF_NAME.into(),
                    ],
                )
                .await
            }
            #[cfg(not(any(target_os = "windows", target_os = "linux")))]
            {
                let _ = cidr;
                Ok(())
            }
        }

        async fn del_cidr_route(&self, cidr: &str) {
            #[cfg(target_os = "windows")]
            let _ = run_cmd(
                "netsh",
                vec![
                    "interface".into(),
                    "ipv4".into(),
                    "delete".into(),
                    "route".into(),
                    format!("prefix={cidr}"),
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
                    cidr.to_string(),
                    "dev".into(),
                    IF_NAME.into(),
                ],
            )
            .await;
            #[cfg(not(any(target_os = "windows", target_os = "linux")))]
            let _ = cidr;
        }
    }

    /// The overlay NIC name we set in [`SystemTun::up`] — used to target
    /// per-peer `/32` routes without a LUID/index lookup.
    #[cfg(target_os = "windows")]
    const IF_NAME: &str = "roomler";
    #[cfg(target_os = "linux")]
    const IF_NAME: &str = "roomler0";

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
}

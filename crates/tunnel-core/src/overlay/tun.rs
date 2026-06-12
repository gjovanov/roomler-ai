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
                run_cmd(
                    "netsh",
                    vec![
                        "interface".into(),
                        "ipv4".into(),
                        "add".into(),
                        "route".into(),
                        format!("prefix={peer}/32"),
                        format!("interface={IF_NAME}"),
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
    }

    /// The overlay NIC name we set in [`SystemTun::up`] — used to target
    /// per-peer `/32` routes without a LUID/index lookup.
    #[cfg(target_os = "windows")]
    const IF_NAME: &str = "roomler";
    #[cfg(target_os = "linux")]
    const IF_NAME: &str = "roomler0";

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

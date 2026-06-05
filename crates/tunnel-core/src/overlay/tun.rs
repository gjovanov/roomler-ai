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
            Ok(Self { dev: Arc::new(dev) })
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
    }
}

//! TUN ↔ WireGuard bridge (Phase 3).
//!
//! Joins the OS TUN device ([`super::tun::TunIo`]) to the userspace
//! [`WgDevice`](super::wg::WgDevice):
//! * **outbound** — read an IP packet off the TUN → `send_ip_packet`
//!   (route by dst overlay IP → peer → encapsulate → carrier);
//! * **inbound** — drain the device's decrypted-packet channel → write
//!   it back to the TUN.
//!
//! `TunIo` keeps the loop device-agnostic, so the loopback test below
//! drives the whole `app → TUN → encrypt → carrier → decrypt → TUN →
//! app` path between two real `WgDevice`s with an in-memory mock NIC — no
//! kernel driver, no privilege.

use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::debug;

use super::tun::TunIo;
use super::wg::WgDevice;

/// Pump packets between `tun` and `dev` until either side closes. Runs
/// the outbound loop inline and spawns the inbound loop; returns when the
/// TUN read side ends (device gone / shutdown). The caller owns task
/// placement (e.g. `tokio::spawn` this, or `select!` it against a
/// shutdown signal). `tun_rx` is the decrypted-inbound channel returned
/// by [`WgDevice::new`](super::wg::WgDevice::new).
pub async fn run_bridge(
    tun: Arc<dyn TunIo>,
    dev: Arc<WgDevice>,
    mut tun_rx: mpsc::Receiver<Vec<u8>>,
) {
    // inbound: decrypted WG packets → TUN.
    let write_tun = tun.clone();
    let inbound = tokio::spawn(async move {
        while let Some(pkt) = tun_rx.recv().await {
            if let Err(e) = write_tun.write_packet(&pkt).await {
                debug!(%e, "overlay bridge: TUN write failed; inbound loop exiting");
                break;
            }
        }
    });

    // outbound: TUN reads → WG encapsulate + send. Best-effort: a packet
    // to an overlay address with no peer/session is dropped (the inner
    // transport retransmits), exactly like a real NIC dropping toward a
    // down route.
    loop {
        match tun.read_packet().await {
            Ok(pkt) => {
                let _ = dev.send_ip_packet(&pkt).await;
            }
            Err(e) => {
                debug!(%e, "overlay bridge: TUN read ended; outbound loop exiting");
                break;
            }
        }
    }

    inbound.abort();
}

#[cfg(test)]
mod tests {
    //! Phase 3a proof: an IP packet injected on one node's mock TUN
    //! surfaces on the peer's mock TUN, having traversed the full bridge +
    //! WireGuard path over a direct UDP carrier — no real device, no
    //! privilege. Mirrors the carrier setup of the `wg` module tests.

    use super::*;
    use crate::overlay::WgKeypair;
    use crate::overlay::wg::{Carrier, WgDevice};
    use std::io;
    use std::net::Ipv4Addr;
    use std::time::Duration;
    use tokio::net::UdpSocket;
    use tokio::sync::Mutex;

    /// In-memory TUN modelling a host's stack: `inject` is what the "app"
    /// sends (read by the bridge as outbound traffic); `delivered` is
    /// what the bridge writes back (decrypted inbound — what the OS/app
    /// would receive).
    struct MockTun {
        inject: Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
        delivered: mpsc::UnboundedSender<Vec<u8>>,
    }

    impl MockTun {
        fn new() -> (
            Arc<Self>,
            mpsc::UnboundedSender<Vec<u8>>,
            mpsc::UnboundedReceiver<Vec<u8>>,
        ) {
            let (inject_tx, inject_rx) = mpsc::unbounded_channel();
            let (delivered_tx, delivered_rx) = mpsc::unbounded_channel();
            (
                Arc::new(Self {
                    inject: Mutex::new(inject_rx),
                    delivered: delivered_tx,
                }),
                inject_tx,
                delivered_rx,
            )
        }
    }

    #[async_trait::async_trait]
    impl TunIo for MockTun {
        async fn read_packet(&self) -> io::Result<Vec<u8>> {
            self.inject
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| io::Error::other("mock tun inject channel closed"))
        }
        async fn write_packet(&self, packet: &[u8]) -> io::Result<()> {
            self.delivered
                .send(packet.to_vec())
                .map_err(|_| io::Error::other("mock tun delivered channel closed"))
        }
    }

    /// Minimal well-formed IPv4 packet (version nibble + total-length +
    /// dst at bytes 16..20) so it routes by destination.
    fn synthetic_ipv4(src: Ipv4Addr, dst: Ipv4Addr, payload: &[u8]) -> Vec<u8> {
        let total = 20 + payload.len();
        let mut p = vec![0u8; total];
        p[0] = 0x45; // IPv4, IHL=5
        p[2] = (total >> 8) as u8;
        p[3] = (total & 0xff) as u8;
        p[8] = 64; // TTL
        p[9] = 17; // UDP
        p[12..16].copy_from_slice(&src.octets());
        p[16..20].copy_from_slice(&dst.octets());
        p[20..].copy_from_slice(payload);
        p
    }

    const IP_A: Ipv4Addr = Ipv4Addr::new(100, 64, 0, 1);
    const IP_B: Ipv4Addr = Ipv4Addr::new(100, 64, 0, 2);

    #[tokio::test(flavor = "multi_thread")]
    async fn bridge_round_trips_ip_packet_app_to_app() {
        let a = WgKeypair::generate();
        let b = WgKeypair::generate();

        let sock_a = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let sock_b = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr_a = sock_a.local_addr().unwrap();
        let addr_b = sock_b.local_addr().unwrap();

        let (mut dev_a, rx_a) = WgDevice::new(a.secret.clone());
        let (mut dev_b, rx_b) = WgDevice::new(b.secret.clone());
        dev_a.add_peer(
            b.public.to_bytes(),
            IP_B,
            Carrier::direct(sock_a.clone(), addr_b),
            true,
        );
        dev_b.add_peer(
            a.public.to_bytes(),
            IP_A,
            Carrier::direct(sock_b.clone(), addr_a),
            false,
        );
        let dev_a = Arc::new(dev_a);
        let dev_b = Arc::new(dev_b);

        let (mock_a, inject_a, _delivered_a) = MockTun::new();
        let (mock_b, _inject_b, mut delivered_b) = MockTun::new();

        tokio::spawn(run_bridge(
            mock_a.clone() as Arc<dyn TunIo>,
            dev_a.clone(),
            rx_a,
        ));
        tokio::spawn(run_bridge(
            mock_b.clone() as Arc<dyn TunIo>,
            dev_b.clone(),
            rx_b,
        ));

        let pkt = synthetic_ipv4(IP_A, IP_B, b"bridge-loopback");

        // The bridge's outbound send is best-effort (drops until the WG
        // session is up), so re-inject and poll until B delivers it.
        for _ in 0..100 {
            let _ = inject_a.send(pkt.clone());
            if let Ok(Some(got)) =
                tokio::time::timeout(Duration::from_millis(150), delivered_b.recv()).await
            {
                assert_eq!(got, pkt, "packet must traverse the bridge intact");
                return;
            }
        }
        panic!("packet did not traverse the bridge in time");
    }
}

//! SOCKS5-CONNECT front for the [netstack](super::netstack).
//!
//! The app-facing half of the OS-free path: a localhost SOCKS5 proxy whose
//! CONNECT opens a smoltcp TCP socket to the target overlay address via
//! [`NetstackHandle::connect`], then splices the client TCP stream to it. This
//! is what makes the mesh reachable from a locked-down host with **no OS
//! routing** — apps point at `127.0.0.1:<port>` and address overlay peers by
//! IP; nothing touches the routing table the VPN captures.
//!
//! v1 scope: CONNECT to an **IPv4 overlay address**. DOMAIN targets (MagicDNS
//! names → overlay IP) and IPv6 are follow-ups; BIND / UDP-ASSOCIATE are out of
//! scope for the netstack front (the OS-free path is TCP-first).

use std::net::{Ipv4Addr, SocketAddrV4};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tracing::{debug, warn};

use super::netstack::NetstackHandle;

// SOCKS5 wire constants (RFC 1928) — mirrors `roomler-tunnel`'s socks5.rs.
const VER: u8 = 0x05;
const CMD_CONNECT: u8 = 0x01;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;
const REP_SUCCESS: u8 = 0x00;
const REP_HOST_UNREACHABLE: u8 = 0x04;
const REP_CMD_NOT_SUPPORTED: u8 = 0x07;
const REP_ATYP_NOT_SUPPORTED: u8 = 0x08;

/// Serve SOCKS5 on `listener`, dialing every CONNECT into **the current**
/// netstack. `handles` tracks the live [`NetstackHandle`] — `None` before the
/// first netmap and re-published on overlay reconnect (a fresh stack) — so the
/// front outlives the runtime's connect/reconnect cycle without rebinding the
/// port. Runs until the listener errors fatally; per-connection failures are
/// logged and dropped. Bind the listener to **loopback only** — the proxy is
/// never exposed on the overlay/LAN.
pub async fn serve_socks5(handles: watch::Receiver<Option<NetstackHandle>>, listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((client, _peer)) => {
                // Snapshot the current netstack; a client that arrives before
                // the mesh is up (or mid-reconnect) is simply dropped.
                let Some(h) = handles.borrow().clone() else {
                    debug!("netstack socks: no netstack up yet; dropping client");
                    continue;
                };
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(h, client).await {
                        debug!(error = %e, "netstack socks: connection ended");
                    }
                });
            }
            Err(e) => {
                warn!(error = %e, "netstack socks: accept failed; stopping");
                break;
            }
        }
    }
}

/// One SOCKS5 client: no-auth handshake → CONNECT → dial via the netstack →
/// splice.
async fn handle_connection(handle: NetstackHandle, mut client: TcpStream) -> std::io::Result<()> {
    // ── greeting: VER, NMETHODS, METHODS ──
    let mut head = [0u8; 2];
    client.read_exact(&mut head).await?;
    if head[0] != VER {
        return Err(std::io::Error::other("not SOCKS5"));
    }
    let mut methods = vec![0u8; head[1] as usize];
    client.read_exact(&mut methods).await?;
    // Only no-auth (0x00) is offered.
    client.write_all(&[VER, 0x00]).await?;

    // ── request: VER, CMD, RSV, ATYP, DST.ADDR, DST.PORT ──
    let mut req = [0u8; 4];
    client.read_exact(&mut req).await?;
    if req[0] != VER {
        return Err(std::io::Error::other("bad SOCKS5 request version"));
    }
    if req[1] != CMD_CONNECT {
        reply(&mut client, REP_CMD_NOT_SUPPORTED).await;
        return Err(std::io::Error::other("only CONNECT is supported"));
    }
    let dst = match req[3] {
        ATYP_IPV4 => {
            let mut a = [0u8; 4];
            client.read_exact(&mut a).await?;
            let mut p = [0u8; 2];
            client.read_exact(&mut p).await?;
            SocketAddrV4::new(Ipv4Addr::from(a), u16::from_be_bytes(p))
        }
        ATYP_DOMAIN => {
            // Drain the framed address so the stream stays aligned, then reject:
            // the netstack front has no resolver yet (MagicDNS name → overlay IP
            // is a follow-up). Address peers by IP for now.
            let mut len = [0u8; 1];
            client.read_exact(&mut len).await?;
            let mut rest = vec![0u8; len[0] as usize + 2]; // name + 2-byte port
            client.read_exact(&mut rest).await?;
            reply(&mut client, REP_ATYP_NOT_SUPPORTED).await;
            return Err(std::io::Error::other("domain targets not supported yet"));
        }
        ATYP_IPV6 => {
            let mut rest = [0u8; 16 + 2];
            client.read_exact(&mut rest).await?;
            reply(&mut client, REP_ATYP_NOT_SUPPORTED).await;
            return Err(std::io::Error::other("IPv6 targets not supported yet"));
        }
        _ => {
            reply(&mut client, REP_ATYP_NOT_SUPPORTED).await;
            return Err(std::io::Error::other("unknown ATYP"));
        }
    };

    // ── dial the target through the netstack ──
    let mut upstream = match handle.connect(dst).await {
        Ok(s) => s,
        Err(e) => {
            reply(&mut client, REP_HOST_UNREACHABLE).await;
            return Err(e);
        }
    };
    reply(&mut client, REP_SUCCESS).await;

    // ── splice client ⇄ netstack until either side closes ──
    tokio::io::copy_bidirectional(&mut client, &mut upstream)
        .await
        .map(|_| ())
}

/// SOCKS5 reply with a zero BND.ADDR/PORT (`IPv4 0.0.0.0:0`, which every client
/// accepts). Best-effort — a write failure just means the client vanished.
async fn reply(client: &mut TcpStream, rep: u8) {
    let _ = client
        .write_all(&[VER, rep, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
        .await;
}

#[cfg(test)]
mod tests {
    //! End-to-end: a real TCP client → SOCKS5 → netstack A → (direct L3
    //! cross-link) → netstack B's echo listener → back. Proves the SOCKS front
    //! drives the netstack; the WG-bridge leg is already covered by
    //! `netstack::tests::bridge_tcp_echo_over_wireguard`.

    use super::*;
    use crate::overlay::netstack::{Netstack, NsTcpStream};
    use crate::overlay::tun::TunIo;
    use std::time::Duration;

    /// Pump every packet A emits into B and vice-versa (L3 loopback, no WG).
    fn crosslink(a: &Netstack, b: &Netstack) {
        let (a1, b1) = (a.tun.clone(), b.tun.clone());
        tokio::spawn(async move {
            while let Ok(p) = a1.read_packet().await {
                if b1.write_packet(&p).await.is_err() {
                    break;
                }
            }
        });
        let (a2, b2) = (a.tun.clone(), b.tun.clone());
        tokio::spawn(async move {
            while let Ok(p) = b2.read_packet().await {
                if a2.write_packet(&p).await.is_err() {
                    break;
                }
            }
        });
    }

    async fn echo(stream: NsTcpStream) {
        let (mut r, mut w) = tokio::io::split(stream);
        let _ = tokio::io::copy(&mut r, &mut w).await;
        let _ = w.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn socks5_connect_over_netstack() {
        let a_ip = Ipv4Addr::new(10, 20, 0, 1);
        let b_ip = Ipv4Addr::new(10, 20, 0, 2);
        let a = Netstack::start(a_ip, 24, 1280);
        let b = Netstack::start(b_ip, 24, 1280);
        crosslink(&a, &b);

        // B echoes on 4000.
        let mut listener = b.handle.listen(4000).await.unwrap();
        tokio::spawn(async move {
            if let Some(s) = listener.accept().await {
                echo(s).await;
            }
        });

        // A's SOCKS front on a real loopback port, backed by A's live netstack.
        let socks = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socks_addr = socks.local_addr().unwrap();
        let (handle_tx, handle_rx) = watch::channel(Some(a.handle.clone()));
        tokio::spawn(serve_socks5(handle_rx, socks));
        // Keep the netstack + watch sender alive for the test's duration.
        let _keep = (a, handle_tx);

        let body = async {
            let mut c = TcpStream::connect(socks_addr).await.unwrap();
            // greeting: VER, NMETHODS=1, METHOD=no-auth
            c.write_all(&[VER, 0x01, 0x00]).await.unwrap();
            let mut mr = [0u8; 2];
            c.read_exact(&mut mr).await.unwrap();
            assert_eq!(mr, [VER, 0x00], "server must select no-auth");
            // CONNECT to b_ip:4000
            let ip = b_ip.octets();
            let port = 4000u16.to_be_bytes();
            c.write_all(&[
                VER,
                CMD_CONNECT,
                0x00,
                ATYP_IPV4,
                ip[0],
                ip[1],
                ip[2],
                ip[3],
                port[0],
                port[1],
            ])
            .await
            .unwrap();
            let mut rep = [0u8; 10];
            c.read_exact(&mut rep).await.unwrap();
            assert_eq!(rep[1], REP_SUCCESS, "CONNECT must succeed");
            // payload echoes back through the netstack
            c.write_all(b"socks-over-netstack").await.unwrap();
            let mut got = vec![0u8; b"socks-over-netstack".len()];
            c.read_exact(&mut got).await.unwrap();
            assert_eq!(&got, b"socks-over-netstack");
        };
        tokio::time::timeout(Duration::from_secs(10), body)
            .await
            .expect("socks round trip in time");
    }
}

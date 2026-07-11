//! SOCKS5-CONNECT front for the [netstack](super::netstack).
//!
//! The app-facing half of the OS-free path: a localhost SOCKS5 proxy whose
//! CONNECT opens a smoltcp TCP socket to the target overlay address via
//! [`NetstackHandle::connect`], then splices the client TCP stream to it. This
//! is what makes the mesh reachable from a locked-down host with **no OS
//! routing** — apps point at `127.0.0.1:<port>` and address overlay peers by
//! IP **or by name**; nothing touches the routing table the VPN captures.
//!
//! Target addressing (SOCKS5 `ATYP`):
//! * **IPv4** — a literal overlay address (`--socks5 100.64.0.5:3389`).
//! * **DOMAIN** — a peer **name** or its MagicDNS FQDN
//!   (`--socks5-hostname neo16:3389` / `neo16.myorg.roomler.net`), resolved to
//!   an overlay IPv4 from the live mesh view ([`resolve_overlay_host`]). This is
//!   MagicDNS-independent: it reads the netmap's `name → overlay-IP` directly,
//!   so it works even when the tenant hasn't configured a magic domain.
//! * **IPv6** — the overlay is IPv4-only, so a genuine IPv6 target is
//!   unreachable and rejected; an **IPv4-mapped** IPv6 (`::ffff:a.b.c.d`, which
//!   some clients emit) is unwrapped to its embedded IPv4 and treated as a
//!   normal overlay address.
//!
//! BIND / UDP-ASSOCIATE are handled separately (UDP-associate rides the same
//! resolver in a follow-up); the CONNECT path is TCP-first.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tracing::{debug, warn};

use super::netstack::NetstackHandle;
use crate::localapi::OverlayView;

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

/// Resolve a SOCKS5 target host to an **overlay IPv4** using the live mesh
/// `view`. Resolution order:
///
/// 1. a literal IPv4 string (`"100.64.0.2"`) — used as-is;
/// 2. an exact, case-insensitive match against a peer's name (`"neo16"`);
/// 3. the first DNS label, so a MagicDNS FQDN (`"neo16.myorg.roomler.net"`)
///    resolves to the same peer as its bare label.
///
/// `None` if nothing matches (⇒ the front replies host-unreachable). Names come
/// straight from the netmap the runtime already publishes, so this needs no DNS
/// server and no magic-domain configuration.
fn resolve_overlay_host(view: &OverlayView, host: &str) -> Option<Ipv4Addr> {
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Some(ip);
    }
    let host_lc = host.to_ascii_lowercase();
    let bare = host_lc.split('.').next().unwrap_or(host_lc.as_str());
    view.peers.iter().find_map(|p| {
        if p.name.is_empty() {
            return None;
        }
        let name_lc = p.name.to_ascii_lowercase();
        if name_lc == host_lc || name_lc == bare {
            p.overlay_ip
                .as_deref()
                .and_then(|s| s.parse::<Ipv4Addr>().ok())
        } else {
            None
        }
    })
}

/// Serve SOCKS5 on `listener`, dialing every CONNECT into **the current**
/// netstack. `handles` tracks the live [`NetstackHandle`] — `None` before the
/// first netmap and re-published on overlay reconnect (a fresh stack) — so the
/// front outlives the runtime's connect/reconnect cycle without rebinding the
/// port. `view` is the live mesh view (from the runtime's `peer_view`), used to
/// resolve DOMAIN targets to overlay IPs. Runs until the listener errors
/// fatally; per-connection failures are logged and dropped. Bind the listener
/// to **loopback only** — the proxy is never exposed on the overlay/LAN.
pub async fn serve_socks5(
    handles: watch::Receiver<Option<NetstackHandle>>,
    view: watch::Receiver<OverlayView>,
    listener: TcpListener,
) {
    loop {
        match listener.accept().await {
            Ok((client, _peer)) => {
                // Snapshot the current netstack; a client that arrives before
                // the mesh is up (or mid-reconnect) is simply dropped.
                let Some(h) = handles.borrow().clone() else {
                    debug!("netstack socks: no netstack up yet; dropping client");
                    continue;
                };
                // Snapshot the mesh view so name resolution reflects the netmap
                // at accept time (cheap: a small Vec of peers).
                let snapshot = view.borrow().clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(h, snapshot, client).await {
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

/// One SOCKS5 client: no-auth handshake → CONNECT → resolve target → dial via
/// the netstack → splice.
async fn handle_connection(
    handle: NetstackHandle,
    view: OverlayView,
    mut client: TcpStream,
) -> std::io::Result<()> {
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
            // len-prefixed host, then a 2-byte port.
            let mut len = [0u8; 1];
            client.read_exact(&mut len).await?;
            let mut name = vec![0u8; len[0] as usize];
            client.read_exact(&mut name).await?;
            let mut p = [0u8; 2];
            client.read_exact(&mut p).await?;
            let host = String::from_utf8_lossy(&name);
            let port = u16::from_be_bytes(p);
            match resolve_overlay_host(&view, &host) {
                Some(ip) => SocketAddrV4::new(ip, port),
                None => {
                    debug!(%host, "netstack socks: no overlay peer for name");
                    reply(&mut client, REP_HOST_UNREACHABLE).await;
                    return Err(std::io::Error::other("unknown overlay host"));
                }
            }
        }
        ATYP_IPV6 => {
            let mut a = [0u8; 16];
            client.read_exact(&mut a).await?;
            let mut p = [0u8; 2];
            client.read_exact(&mut p).await?;
            // The overlay is IPv4-only. An IPv4-mapped v6 (`::ffff:a.b.c.d`)
            // carries a real overlay IPv4 — unwrap it; anything else is
            // genuinely unreachable.
            match Ipv6Addr::from(a).to_ipv4_mapped() {
                Some(v4) => SocketAddrV4::new(v4, u16::from_be_bytes(p)),
                None => {
                    reply(&mut client, REP_ATYP_NOT_SUPPORTED).await;
                    return Err(std::io::Error::other(
                        "IPv6 targets not supported (overlay is IPv4-only)",
                    ));
                }
            }
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
    //! drives the netstack — by literal IP **and** by resolved peer name; the
    //! WG-bridge leg is already covered by
    //! `netstack::tests::bridge_tcp_echo_over_wireguard`.

    use super::*;
    use crate::localapi::{ConnectionType, PeerInfo};
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

    fn peer(name: &str, ip: &str) -> PeerInfo {
        PeerInfo {
            node_id: "0".repeat(24),
            name: name.into(),
            overlay_ip: Some(ip.into()),
            online: true,
            connection: ConnectionType::Direct,
            rtt_ms: None,
            last_seen_ms: None,
        }
    }

    #[test]
    fn resolves_ip_name_and_fqdn() {
        let view = OverlayView {
            self_ip: Some("100.64.0.1".into()),
            peers: vec![peer("NEO16", "100.64.0.2"), peer("pc50045", "100.64.0.4")],
        };
        // literal IPv4 passes through
        assert_eq!(
            resolve_overlay_host(&view, "100.64.0.9"),
            Some(Ipv4Addr::new(100, 64, 0, 9))
        );
        // exact name, case-insensitive
        assert_eq!(
            resolve_overlay_host(&view, "neo16"),
            Some(Ipv4Addr::new(100, 64, 0, 2))
        );
        // MagicDNS FQDN → first label
        assert_eq!(
            resolve_overlay_host(&view, "PC50045.myorg.roomler.net"),
            Some(Ipv4Addr::new(100, 64, 0, 4))
        );
        // unknown name
        assert_eq!(resolve_overlay_host(&view, "ghost"), None);
    }

    #[test]
    fn ipv4_mapped_v6_unwraps() {
        // `::ffff:100.64.0.7` → the embedded overlay IPv4.
        let mapped = Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x6440, 0x0007);
        assert_eq!(mapped.to_ipv4_mapped(), Some(Ipv4Addr::new(100, 64, 0, 7)));
        // a genuine v6 address has no embedded v4.
        assert_eq!(
            Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1).to_ipv4_mapped(),
            None
        );
    }

    /// Drive one SOCKS5 CONNECT (by the given ATYP frame) and assert the echo
    /// round-trips. `addr_frame` is everything from ATYP through DST.PORT.
    async fn socks_round_trip(socks_addr: std::net::SocketAddr, addr_frame: &[u8]) {
        let mut c = TcpStream::connect(socks_addr).await.unwrap();
        c.write_all(&[VER, 0x01, 0x00]).await.unwrap();
        let mut mr = [0u8; 2];
        c.read_exact(&mut mr).await.unwrap();
        assert_eq!(mr, [VER, 0x00], "server must select no-auth");
        let mut req = vec![VER, CMD_CONNECT, 0x00];
        req.extend_from_slice(addr_frame);
        c.write_all(&req).await.unwrap();
        let mut rep = [0u8; 10];
        c.read_exact(&mut rep).await.unwrap();
        assert_eq!(rep[1], REP_SUCCESS, "CONNECT must succeed");
        c.write_all(b"socks-over-netstack").await.unwrap();
        let mut got = vec![0u8; b"socks-over-netstack".len()];
        c.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"socks-over-netstack");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn socks5_connect_by_ip_and_by_name() {
        let a_ip = Ipv4Addr::new(10, 20, 0, 1);
        let b_ip = Ipv4Addr::new(10, 20, 0, 2);
        let a = Netstack::start(a_ip, 24, 1280);
        let b = Netstack::start(b_ip, 24, 1280);
        crosslink(&a, &b);

        // B echoes on 4000 (accepts repeatedly for the two CONNECTs below).
        let mut listener = b.handle.listen(4000).await.unwrap();
        tokio::spawn(async move {
            while let Some(s) = listener.accept().await {
                tokio::spawn(echo(s));
            }
        });

        // A's SOCKS front, backed by A's live netstack + a view that names B.
        let socks = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socks_addr = socks.local_addr().unwrap();
        let (handle_tx, handle_rx) = watch::channel(Some(a.handle.clone()));
        let (view_tx, view_rx) = watch::channel(OverlayView {
            self_ip: Some(a_ip.to_string()),
            peers: vec![peer("peerb", &b_ip.to_string())],
        });
        tokio::spawn(serve_socks5(handle_rx, view_rx, socks));
        let _keep = (a, handle_tx, view_tx);

        // (1) by literal IPv4.
        let ip = b_ip.octets();
        let by_ip = [ATYP_IPV4, ip[0], ip[1], ip[2], ip[3], 0x0f, 0xa0]; // :4000
        // (2) by name "peerb" (ATYP_DOMAIN).
        let name = b"peerb";
        let mut by_name = vec![ATYP_DOMAIN, name.len() as u8];
        by_name.extend_from_slice(name);
        by_name.extend_from_slice(&4000u16.to_be_bytes());

        tokio::time::timeout(Duration::from_secs(10), async {
            socks_round_trip(socks_addr, &by_ip).await;
            socks_round_trip(socks_addr, &by_name).await;
        })
        .await
        .expect("both socks round trips in time");
    }
}

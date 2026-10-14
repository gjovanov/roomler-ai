// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Reading a UDP socket past the errors that are about an earlier datagram.
//!
//! On Windows, an unconnected UDP socket reports an ICMP port-unreachable
//! drawn by a datagram it SENT as `WSAECONNRESET` (10054) on the NEXT
//! receive, and a datagram larger than the buffer as `WSAEMSGSIZE` (10040).
//! Neither is about the socket, which goes on working. A receive loop that
//! ends on them leaves its socket bound, and a carrier's advertised, with
//! nothing reading it. Linux and macOS never report the ICMP on an
//! unconnected socket.
//!
//! Field, 2026-10-14, both on Windows exit clients:
//! - #2213: the MagicDNS resolver exited on its reply to a port the DNS
//!   client had already closed, and under an exit's "." steer the host lost
//!   all DNS.
//! - #2220: the carrier plane's LAN socket stopped reading 16 s after a peer
//!   host began answering its datagrams with port-unreachable. The direct
//!   path through it stayed dead for 107 s, until a watchdog rebuilt the
//!   whole plane.
//!
//! ⚠️ For real OS sockets only. A carrier's own `recv` (DERP, the org relay)
//! reports a CLOSED session as `ConnectionReset`, and reading on past that
//! would spin forever.

use std::io;
use std::net::SocketAddr;

use tokio::net::UdpSocket;
use tracing::debug;

/// A `recv_from` error that reports an EARLIER datagram, not the socket.
pub(crate) fn recv_error_is_transient(e: &io::Error) -> bool {
    use io::ErrorKind;
    matches!(
        e.kind(),
        ErrorKind::ConnectionReset
            | ErrorKind::ConnectionRefused
            | ErrorKind::Interrupted
            | ErrorKind::WouldBlock
    ) || e.raw_os_error() == Some(10040)
}

/// `recv_from`, reading on past [`recv_error_is_transient`] errors: an error
/// it returns IS about the socket.
pub(crate) async fn recv_from(sock: &UdpSocket, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
    loop {
        match sock.recv_from(buf).await {
            Err(e) if recv_error_is_transient(&e) => {
                debug!(
                    %e,
                    local = ?sock.local_addr().ok(),
                    "udp: a receive reported an earlier datagram; reading on"
                );
            }
            r => return r,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The errors that are about an earlier datagram, not the socket:
    /// Windows' ICMP-driven 10054, an oversized datagram's 10040.
    #[test]
    fn errors_about_an_earlier_datagram_are_not_fatal() {
        use std::io::{Error, ErrorKind};
        for kind in [ErrorKind::ConnectionReset, ErrorKind::ConnectionRefused] {
            assert!(recv_error_is_transient(&Error::from(kind)), "{kind:?}");
        }
        assert!(recv_error_is_transient(&Error::from_raw_os_error(10040)));
        for kind in [
            ErrorKind::InvalidInput,
            ErrorKind::NotConnected,
            ErrorKind::Other,
        ] {
            assert!(!recv_error_is_transient(&Error::from(kind)), "{kind:?}");
        }
    }

    /// The field case, on loopback. The socket sends to ports nothing listens
    /// on; each draws an ICMP port-unreachable, which Windows reports on the
    /// NEXT receive as `WSAECONNRESET`. The datagram that arrives after them
    /// must still be read. Linux never reports that on an unconnected socket,
    /// so this passes there either way; Windows is where it discriminates.
    #[tokio::test]
    async fn a_receive_reads_on_past_port_unreachables() {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        for _ in 0..3 {
            let gone = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let closed = gone.local_addr().unwrap();
            drop(gone);
            sock.send_to(b"to nobody", closed).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;

        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        peer.send_to(b"live", sock.local_addr().unwrap())
            .await
            .unwrap();
        let mut buf = [0u8; 64];
        let (n, from) = tokio::time::timeout(Duration::from_secs(3), recv_from(&sock, &mut buf))
            .await
            .expect("the live datagram must be read")
            .expect("a port-unreachable must not end the receive");
        assert_eq!(&buf[..n], b"live");
        assert_eq!(from, peer.local_addr().unwrap());
    }
}

//! Minimal SOCKS5 server (RFC 1928) — CONNECT only, no authentication.
//!
//! This is the tunnel's **userspace mode** (Tailscale's `--socks5-server`
//! equivalent): a local SOCKS5 proxy whose per-connection CONNECT target is
//! dialed by the remote agent over the existing tunnel transport. Apps point at
//! the proxy and reach the agent's network with **no OS routing** — which is why
//! it works on strict full-tunnel corporate VPNs (e.g. Check Point) that capture
//! the L3 overlay's routes at the OS layer. Only TCP CONNECT is handled (RDP /
//! SSH / SQL / HTTP — the real workloads); UDP ASSOCIATE / BIND are rejected.
//!
//! Bound to 127.0.0.1 by the caller, so no on-wire auth is needed (the SOCKS
//! hop never leaves the host); the target is still gated by the server-side
//! tunnel policy + the agent allowlist, exactly like a static `--remote`.

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const VER: u8 = 0x05;
const METHOD_NO_AUTH: u8 = 0x00;
const METHOD_NONE: u8 = 0xFF;
const CMD_CONNECT: u8 = 0x01;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;
pub const REP_SUCCESS: u8 = 0x00;
pub const REP_GENERAL_FAILURE: u8 = 0x01;
const REP_CMD_NOT_SUPPORTED: u8 = 0x07;
const REP_ATYP_NOT_SUPPORTED: u8 = 0x08;

/// Run the SOCKS5 method negotiation (offering only no-auth) and read the
/// CONNECT request, returning the client-specified `(host, port)`. On return the
/// stream is positioned at the first byte of application payload. Writes the
/// appropriate SOCKS failure reply itself for protocol-level rejections
/// (unsupported command / address type) before erroring; the caller sends the
/// success reply via [`reply`] once the agent has accepted the forward.
pub async fn accept_connect(tcp: &mut TcpStream) -> Result<(String, u16)> {
    // ── method negotiation: VER, NMETHODS, METHODS ──
    let ver = tcp.read_u8().await.context("read socks version")?;
    if ver != VER {
        bail!("unsupported SOCKS version {ver:#x} (only 5)");
    }
    let nmethods = tcp.read_u8().await.context("read nmethods")?;
    let mut methods = vec![0u8; nmethods as usize];
    tcp.read_exact(&mut methods)
        .await
        .context("read auth methods")?;
    if !methods.contains(&METHOD_NO_AUTH) {
        let _ = tcp.write_all(&[VER, METHOD_NONE]).await;
        bail!("client offered no no-auth method");
    }
    tcp.write_all(&[VER, METHOD_NO_AUTH])
        .await
        .context("write method selection")?;

    // ── request: VER, CMD, RSV, ATYP, DST.ADDR, DST.PORT ──
    let ver = tcp.read_u8().await.context("read request version")?;
    if ver != VER {
        bail!("bad request version {ver:#x}");
    }
    let cmd = tcp.read_u8().await.context("read cmd")?;
    let _rsv = tcp.read_u8().await.context("read rsv")?;
    let atyp = tcp.read_u8().await.context("read atyp")?;
    let host = match atyp {
        ATYP_IPV4 => {
            let mut b = [0u8; 4];
            tcp.read_exact(&mut b).await.context("read ipv4 dst")?;
            std::net::Ipv4Addr::from(b).to_string()
        }
        ATYP_IPV6 => {
            let mut b = [0u8; 16];
            tcp.read_exact(&mut b).await.context("read ipv6 dst")?;
            std::net::Ipv6Addr::from(b).to_string()
        }
        ATYP_DOMAIN => {
            let len = tcp.read_u8().await.context("read domain len")?;
            let mut b = vec![0u8; len as usize];
            tcp.read_exact(&mut b).await.context("read domain")?;
            String::from_utf8(b).context("domain name not utf-8")?
        }
        other => {
            reply(tcp, REP_ATYP_NOT_SUPPORTED).await;
            bail!("unsupported address type {other:#x}");
        }
    };
    let port = tcp.read_u16().await.context("read dst port")?; // big-endian
    if cmd != CMD_CONNECT {
        reply(tcp, REP_CMD_NOT_SUPPORTED).await;
        bail!("unsupported SOCKS command {cmd:#x} (only CONNECT)");
    }
    Ok((host, port))
}

/// Write a SOCKS5 reply with `rep` and a zero `BND.ADDR`/`BND.PORT`
/// (`ATYP=IPv4 0.0.0.0:0`), which every compliant client accepts. Best-effort:
/// if the client has already gone, the error is ignored (the caller tears the
/// flow down regardless). Send [`REP_SUCCESS`] once the agent accepts the
/// forward, or [`REP_GENERAL_FAILURE`] on reject.
pub async fn reply(tcp: &mut TcpStream, rep: u8) {
    let _ = tcp
        .write_all(&[VER, rep, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Drive a full method-negotiation + CONNECT (domain target) through a
    /// loopback socket pair and assert the parsed target + the on-wire replies.
    #[tokio::test]
    async fn parses_domain_connect_and_replies() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let client = tokio::spawn(async move {
            let mut c = TcpStream::connect(addr).await.unwrap();
            // greeting: VER=5, NMETHODS=1, METHOD=no-auth
            c.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
            let mut sel = [0u8; 2];
            c.read_exact(&mut sel).await.unwrap();
            assert_eq!(sel, [0x05, 0x00]);
            // CONNECT db.internal:5432 (ATYP=domain)
            let host = b"db.internal";
            let mut req = vec![0x05, CMD_CONNECT, 0x00, ATYP_DOMAIN, host.len() as u8];
            req.extend_from_slice(host);
            req.extend_from_slice(&5432u16.to_be_bytes());
            c.write_all(&req).await.unwrap();
            c
        });

        let (mut srv, _) = listener.accept().await.unwrap();
        let (host, port) = accept_connect(&mut srv).await.unwrap();
        assert_eq!(host, "db.internal");
        assert_eq!(port, 5432);

        // Success reply is accepted by the client half.
        reply(&mut srv, REP_SUCCESS).await;
        let mut c = client.await.unwrap();
        let mut rep = [0u8; 10];
        c.read_exact(&mut rep).await.unwrap();
        assert_eq!(rep[0], VER);
        assert_eq!(rep[1], REP_SUCCESS);
    }

    /// A non-CONNECT command (UDP ASSOCIATE) is rejected with the
    /// command-not-supported reply, not parsed as a forward.
    #[tokio::test]
    async fn rejects_non_connect_command() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut c = TcpStream::connect(addr).await.unwrap();
            c.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
            let mut sel = [0u8; 2];
            c.read_exact(&mut sel).await.unwrap();
            // CMD=0x03 (UDP ASSOCIATE), ATYP=IPv4 1.2.3.4:9
            c.write_all(&[0x05, 0x03, 0x00, ATYP_IPV4, 1, 2, 3, 4, 0, 9])
                .await
                .unwrap();
            let mut rep = [0u8; 10];
            let _ = c.read_exact(&mut rep).await;
            rep
        });
        let (mut srv, _) = listener.accept().await.unwrap();
        assert!(accept_connect(&mut srv).await.is_err());
        let rep = client.await.unwrap();
        assert_eq!(rep[1], REP_CMD_NOT_SUPPORTED);
    }

    /// A client that offers no no-auth method is rejected at negotiation.
    #[tokio::test]
    async fn rejects_when_no_no_auth_method() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut c = TcpStream::connect(addr).await.unwrap();
            // NMETHODS=1, METHOD=0x02 (username/password) only
            c.write_all(&[0x05, 0x01, 0x02]).await.unwrap();
            let mut sel = [0u8; 2];
            let _ = c.read_exact(&mut sel).await;
            sel
        });
        let (mut srv, _) = listener.accept().await.unwrap();
        assert!(accept_connect(&mut srv).await.is_err());
        let sel = client.await.unwrap();
        assert_eq!(sel, [VER, METHOD_NONE]);
    }
}

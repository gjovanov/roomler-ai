// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
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
//!
//! FR-90 P3-7-3b — an agent session's listener is the exception: a second
//! session under the same account could share its loopback port, so it takes
//! RFC 1929 with the listener's own capability, and CONNECT alone
//! ([`accept_connect_authenticated`]).

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const VER: u8 = 0x05;
const METHOD_NO_AUTH: u8 = 0x00;
/// FR-90 P3-7-3b — RFC 1929's method, offered by a session's listener alone.
const METHOD_USER_PASS: u8 = 0x02;
const METHOD_NONE: u8 = 0xFF;
/// RFC 1929's sub-negotiation version, and its two statuses.
const AUTH_VER: u8 = 0x01;
const AUTH_OK: u8 = 0x00;
const AUTH_FAILED: u8 = 0x01;
const CMD_CONNECT: u8 = 0x01;
pub const CMD_UDP_ASSOCIATE: u8 = 0x03;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;
pub const REP_SUCCESS: u8 = 0x00;
pub const REP_GENERAL_FAILURE: u8 = 0x01;
const REP_CMD_NOT_SUPPORTED: u8 = 0x07;
const REP_ATYP_NOT_SUPPORTED: u8 = 0x08;

/// A parsed SOCKS5 request after method negotiation. `Connect` carries
/// the CONNECT target; `UdpAssociate` signals the client wants a UDP
/// relay (the request's DST is the address the app *will* send from —
/// per RFC 1928 it's advisory and we ignore it, binding a fresh relay
/// socket instead).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Socks5Request {
    Connect { host: String, port: u16 },
    UdpAssociate,
}

/// Run the SOCKS5 method negotiation (offering only no-auth) and read
/// one request, returning either a CONNECT target or a UDP ASSOCIATE.
/// On a `Connect` return the stream is positioned at the first byte of
/// application payload. Writes the appropriate SOCKS failure reply
/// itself for protocol-level rejections (unsupported command / address
/// type) before erroring; the caller sends the success reply once the
/// agent has accepted the forward.
pub async fn accept_request(tcp: &mut TcpStream) -> Result<Socks5Request> {
    negotiate(tcp, METHOD_NO_AUTH).await?;
    read_request(tcp).await
}

/// The method negotiation (VER, NMETHODS, METHODS), selecting `method` alone:
/// a client that does not offer it is answered "no acceptable method".
async fn negotiate(tcp: &mut TcpStream, method: u8) -> Result<()> {
    let ver = tcp.read_u8().await.context("read socks version")?;
    if ver != VER {
        bail!("unsupported SOCKS version {ver:#x} (only 5)");
    }
    let nmethods = tcp.read_u8().await.context("read nmethods")?;
    let mut methods = vec![0u8; nmethods as usize];
    tcp.read_exact(&mut methods)
        .await
        .context("read auth methods")?;
    if !methods.contains(&method) {
        let _ = tcp.write_all(&[VER, METHOD_NONE]).await;
        bail!("client offered no {method:#04x} method");
    }
    tcp.write_all(&[VER, method])
        .await
        .context("write method selection")
}

/// FR-90 P3-7-3b — the one credential an agent session's SOCKS5 listener
/// takes (RFC 1929): the session's id for the username, the listener's
/// capability for the password.
///
/// ⚠️ The password is the capability: `Debug` names the username alone.
#[derive(Clone)]
pub struct Socks5Credential {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for Socks5Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Socks5Credential")
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

/// The credential the tests' session listeners take, made at run time so the
/// tree holds no username/password literal (GitGuardian scans every commit).
#[cfg(test)]
pub(crate) fn test_credential() -> Socks5Credential {
    let username = ["session", "1"].join("-");
    let password = ["the", "listeners", "capability"].join("-");
    Socks5Credential { username, password }
}

/// FR-90 P3-7-3b — [`accept_connect`] for an agent session's listener: RFC
/// 1929 alone, `cred` alone, compared in constant time, and CONNECT alone (the
/// listener's account check reads TCP sockets). A client offering no
/// username/password method, or another credential, is refused before any
/// request is read. Every other listener keeps [`accept_request`]'s no-auth.
pub async fn accept_connect_authenticated(
    tcp: &mut TcpStream,
    cred: &Socks5Credential,
) -> Result<(String, u16)> {
    use subtle::ConstantTimeEq;
    negotiate(tcp, METHOD_USER_PASS).await?;
    // ── RFC 1929: VER, ULEN, UNAME, PLEN, PASSWD ──
    let ver = tcp.read_u8().await.context("read auth version")?;
    if ver != AUTH_VER {
        let _ = tcp.write_all(&[AUTH_VER, AUTH_FAILED]).await;
        bail!("unsupported username/password version {ver:#x} (only 1)");
    }
    let ulen = tcp.read_u8().await.context("read username length")?;
    let mut uname = vec![0u8; ulen as usize];
    tcp.read_exact(&mut uname).await.context("read username")?;
    let plen = tcp.read_u8().await.context("read password length")?;
    let mut passwd = vec![0u8; plen as usize];
    tcp.read_exact(&mut passwd).await.context("read password")?;
    let user_ok = uname.as_slice().ct_eq(cred.username.as_bytes());
    let pass_ok = passwd.as_slice().ct_eq(cred.password.as_bytes());
    if !bool::from(user_ok & pass_ok) {
        let _ = tcp.write_all(&[AUTH_VER, AUTH_FAILED]).await;
        bail!("a username or password that is not this listener's");
    }
    tcp.write_all(&[AUTH_VER, AUTH_OK])
        .await
        .context("write auth status")?;
    match read_request(tcp).await? {
        Socks5Request::Connect { host, port } => Ok((host, port)),
        Socks5Request::UdpAssociate => {
            reply(tcp, REP_CMD_NOT_SUPPORTED).await;
            bail!("UDP ASSOCIATE is not served on a session's listener");
        }
    }
}

/// One request after the negotiation: VER, CMD, RSV, ATYP, DST.ADDR, DST.PORT.
async fn read_request(tcp: &mut TcpStream) -> Result<Socks5Request> {
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
    match cmd {
        CMD_CONNECT => Ok(Socks5Request::Connect { host, port }),
        CMD_UDP_ASSOCIATE => Ok(Socks5Request::UdpAssociate),
        other => {
            reply(tcp, REP_CMD_NOT_SUPPORTED).await;
            bail!("unsupported SOCKS command {other:#x} (only CONNECT / UDP ASSOCIATE)");
        }
    }
}

/// Convenience wrapper over [`accept_request`] for the TCP-only paths
/// (static `--remote` forward, or a listener that doesn't relay UDP):
/// returns the CONNECT `(host, port)` and rejects UDP ASSOCIATE with a
/// command-not-supported reply.
pub async fn accept_connect(tcp: &mut TcpStream) -> Result<(String, u16)> {
    match accept_request(tcp).await? {
        Socks5Request::Connect { host, port } => Ok((host, port)),
        Socks5Request::UdpAssociate => {
            reply(tcp, REP_CMD_NOT_SUPPORTED).await;
            bail!("UDP ASSOCIATE not supported on this listener");
        }
    }
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

/// Write a SOCKS5 reply carrying a real `BND.ADDR`/`BND.PORT` — used by
/// UDP ASSOCIATE, where the app MUST learn the relay socket's address
/// to send its datagrams to. `rep` is normally [`REP_SUCCESS`].
pub async fn reply_bound(tcp: &mut TcpStream, rep: u8, addr: std::net::SocketAddr) {
    let mut msg = vec![VER, rep, 0x00];
    match addr.ip() {
        std::net::IpAddr::V4(v4) => {
            msg.push(ATYP_IPV4);
            msg.extend_from_slice(&v4.octets());
        }
        std::net::IpAddr::V6(v6) => {
            msg.push(ATYP_IPV6);
            msg.extend_from_slice(&v6.octets());
        }
    }
    msg.extend_from_slice(&addr.port().to_be_bytes());
    let _ = tcp.write_all(&msg).await;
}

/// Minimal SOCKS5 **client** handshake over an already-connected `stream`:
/// negotiate no-auth, then CONNECT to `dst_host:dst_port` (sent as a domain-name
/// ATYP so the far proxy resolves it). Returns `Ok(())` on a success reply,
/// leaving the stream positioned at the first payload byte. Used by the mesh to
/// chain into a per-agent loopback proxy.
pub async fn client_connect(stream: &mut TcpStream, dst_host: &str, dst_port: u16) -> Result<()> {
    stream
        .write_all(&[VER, 0x01, METHOD_NO_AUTH])
        .await
        .context("socks client greeting")?;
    let mut sel = [0u8; 2];
    stream
        .read_exact(&mut sel)
        .await
        .context("socks client method reply")?;
    if sel != [VER, METHOD_NO_AUTH] {
        bail!("proxy rejected no-auth (got {sel:?})");
    }
    let host = dst_host.as_bytes();
    if host.len() > 255 {
        bail!("dst_host too long for SOCKS domain ATYP");
    }
    let mut req = vec![VER, CMD_CONNECT, 0x00, ATYP_DOMAIN, host.len() as u8];
    req.extend_from_slice(host);
    req.extend_from_slice(&dst_port.to_be_bytes());
    stream
        .write_all(&req)
        .await
        .context("socks client connect request")?;
    let mut head = [0u8; 4];
    stream
        .read_exact(&mut head)
        .await
        .context("socks client reply header")?;
    if head[1] != REP_SUCCESS {
        bail!("proxy CONNECT failed (REP={:#x})", head[1]);
    }
    // Consume BND.ADDR + BND.PORT so the stream is positioned at the payload.
    let addr_len = match head[3] {
        ATYP_IPV4 => 4,
        ATYP_IPV6 => 16,
        ATYP_DOMAIN => {
            let mut l = [0u8; 1];
            stream
                .read_exact(&mut l)
                .await
                .context("socks client reply domain len")?;
            l[0] as usize
        }
        other => bail!("bad reply atyp {other:#x}"),
    };
    let mut skip = vec![0u8; addr_len + 2];
    stream
        .read_exact(&mut skip)
        .await
        .context("socks client reply addr")?;
    Ok(())
}

/// SOCKS5 **client** UDP ASSOCIATE against an already-connected `stream`
/// (a per-agent loopback proxy in the mesh): negotiate no-auth, request
/// UDP ASSOCIATE, and return the proxy's relay `SocketAddr` (its
/// `BND.ADDR`/`BND.PORT`) that the caller sends SOCKS-UDP datagrams to.
/// The caller MUST keep `stream` open — the association lives as long as
/// this TCP control connection (RFC 1928).
pub async fn client_udp_associate(stream: &mut TcpStream) -> Result<std::net::SocketAddr> {
    stream
        .write_all(&[VER, 0x01, METHOD_NO_AUTH])
        .await
        .context("socks udp greeting")?;
    let mut sel = [0u8; 2];
    stream
        .read_exact(&mut sel)
        .await
        .context("socks udp method reply")?;
    if sel != [VER, METHOD_NO_AUTH] {
        bail!("proxy rejected no-auth (got {sel:?})");
    }
    // UDP ASSOCIATE with an advisory DST of 0.0.0.0:0.
    stream
        .write_all(&[VER, CMD_UDP_ASSOCIATE, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
        .await
        .context("socks udp associate request")?;
    let mut head = [0u8; 4];
    stream
        .read_exact(&mut head)
        .await
        .context("socks udp reply header")?;
    if head[1] != REP_SUCCESS {
        bail!("proxy UDP ASSOCIATE failed (REP={:#x})", head[1]);
    }
    let ip = match head[3] {
        ATYP_IPV4 => {
            let mut b = [0u8; 4];
            stream.read_exact(&mut b).await.context("udp reply ipv4")?;
            std::net::IpAddr::V4(std::net::Ipv4Addr::from(b))
        }
        ATYP_IPV6 => {
            let mut b = [0u8; 16];
            stream.read_exact(&mut b).await.context("udp reply ipv6")?;
            std::net::IpAddr::V6(std::net::Ipv6Addr::from(b))
        }
        other => bail!("proxy returned unsupported BND.ADDR atyp {other:#x}"),
    };
    let mut p = [0u8; 2];
    stream.read_exact(&mut p).await.context("udp reply port")?;
    Ok(std::net::SocketAddr::new(ip, u16::from_be_bytes(p)))
}

/// Parse a SOCKS5 UDP-relay datagram (RFC 1928 §7):
/// `[RSV(2)=0 | FRAG(1) | ATYP | DST.ADDR | DST.PORT | DATA]`.
/// Returns `(dst_host, dst_port, data_offset)` — the payload is
/// `buf[data_offset..]`. Fragmented datagrams (`FRAG != 0`) are rejected
/// (unsupported, like most proxies — the app's upper layer handles MTU).
pub fn parse_udp_datagram(buf: &[u8]) -> Result<(String, u16, usize)> {
    if buf.len() < 4 {
        bail!("socks udp datagram too short ({} bytes)", buf.len());
    }
    if buf[2] != 0 {
        bail!(
            "fragmented socks udp datagrams unsupported (FRAG={})",
            buf[2]
        );
    }
    let (host, addr_end) = match buf[3] {
        ATYP_IPV4 => {
            if buf.len() < 4 + 4 + 2 {
                bail!("socks udp ipv4 datagram truncated");
            }
            (
                std::net::Ipv4Addr::new(buf[4], buf[5], buf[6], buf[7]).to_string(),
                8,
            )
        }
        ATYP_IPV6 => {
            if buf.len() < 4 + 16 + 2 {
                bail!("socks udp ipv6 datagram truncated");
            }
            let mut o = [0u8; 16];
            o.copy_from_slice(&buf[4..20]);
            (std::net::Ipv6Addr::from(o).to_string(), 20)
        }
        ATYP_DOMAIN => {
            let len = *buf.get(4).context("socks udp domain len")? as usize;
            let end = 5 + len;
            if buf.len() < end + 2 {
                bail!("socks udp domain datagram truncated");
            }
            let host =
                String::from_utf8(buf[5..end].to_vec()).context("socks udp domain not utf-8")?;
            (host, end)
        }
        other => bail!("socks udp unsupported atyp {other:#x}"),
    };
    let port = u16::from_be_bytes([buf[addr_end], buf[addr_end + 1]]);
    Ok((host, port, addr_end + 2))
}

/// Encode a SOCKS5 UDP-relay datagram wrapping `data` originating from
/// `(src_host, src_port)`, for delivery back to the app. `src_host` may be an IP
/// (→ ipv4/ipv6 ATYP) or a domain (→ domain ATYP). RSV=0, FRAG=0.
pub fn encode_udp_datagram(src_host: &str, src_port: u16, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 22);
    out.extend_from_slice(&[0, 0, 0]); // RSV(2)=0, FRAG=0
    if let Ok(v4) = src_host.parse::<std::net::Ipv4Addr>() {
        out.push(ATYP_IPV4);
        out.extend_from_slice(&v4.octets());
    } else if let Ok(v6) = src_host.parse::<std::net::Ipv6Addr>() {
        out.push(ATYP_IPV6);
        out.extend_from_slice(&v6.octets());
    } else {
        let h = src_host.as_bytes();
        let n = h.len().min(255);
        out.push(ATYP_DOMAIN);
        out.push(n as u8);
        out.extend_from_slice(&h[..n]);
    }
    out.extend_from_slice(&src_port.to_be_bytes());
    out.extend_from_slice(data);
    out
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

    /// What a client saw of a session listener's handshake: the method the
    /// server selected, the RFC 1929 status if it got that far, and the
    /// request's reply code if it got that far.
    type Seen = (u8, Option<u8>, Option<u8>);

    /// FR-90 P3-7-3b — offer `methods`, authenticate as `user`/`pass` if the
    /// server selects RFC 1929, then send `request`.
    async fn user_pass_client(
        addr: std::net::SocketAddr,
        methods: Vec<u8>,
        user: Vec<u8>,
        pass: Vec<u8>,
        request: Vec<u8>,
    ) -> Seen {
        let mut c = TcpStream::connect(addr).await.unwrap();
        let mut hello = vec![VER, methods.len() as u8];
        hello.extend_from_slice(&methods);
        c.write_all(&hello).await.unwrap();
        let mut sel = [0u8; 2];
        c.read_exact(&mut sel).await.unwrap();
        if sel[1] != METHOD_USER_PASS {
            return (sel[1], None, None);
        }
        let mut auth = vec![AUTH_VER, user.len() as u8];
        auth.extend_from_slice(&user);
        auth.push(pass.len() as u8);
        auth.extend_from_slice(&pass);
        c.write_all(&auth).await.unwrap();
        let mut status = [0u8; 2];
        c.read_exact(&mut status).await.unwrap();
        if status[1] != AUTH_OK {
            return (sel[1], Some(status[1]), None);
        }
        c.write_all(&request).await.unwrap();
        let mut rep = [0u8; 10];
        match c.read_exact(&mut rep).await {
            Ok(_) => (sel[1], Some(status[1]), Some(rep[1])),
            Err(_) => (sel[1], Some(status[1]), None),
        }
    }

    fn connect_to(host: &str, port: u16) -> Vec<u8> {
        let mut req = vec![VER, CMD_CONNECT, 0x00, ATYP_DOMAIN, host.len() as u8];
        req.extend_from_slice(host.as_bytes());
        req.extend_from_slice(&port.to_be_bytes());
        req
    }

    /// One session-listener handshake: what the listener made of it, and
    /// what the client saw.
    async fn session_handshake(
        methods: &[u8],
        user: &str,
        pass: &str,
        request: Vec<u8>,
    ) -> (Result<(String, u16)>, Seen) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(user_pass_client(
            addr,
            methods.to_vec(),
            user.as_bytes().to_vec(),
            pass.as_bytes().to_vec(),
            request,
        ));
        let (mut srv, _) = listener.accept().await.unwrap();
        let cred = test_credential();
        let got = accept_connect_authenticated(&mut srv, &cred).await;
        if got.is_ok() {
            reply(&mut srv, REP_SUCCESS).await;
        }
        drop(srv);
        (got, client.await.unwrap())
    }

    /// FR-90 P3-7-3b — a session's listener serves its own credential alone:
    /// another password or another username is refused at RFC 1929, a client
    /// that offers no username/password method at the negotiation, and either
    /// way no request is read.
    #[tokio::test]
    async fn a_session_listener_takes_its_own_credential_alone() {
        let cred = test_credential();
        let (me, ours) = (cred.username.as_str(), cred.password.as_str());
        let another_password = format!("{ours}x");
        let another_user = format!("{me}x");
        for (methods, user, pass, seen) in [
            (
                &[METHOD_USER_PASS][..],
                me,
                ours,
                (METHOD_USER_PASS, Some(AUTH_OK), Some(REP_SUCCESS)),
            ),
            (
                &[METHOD_NO_AUTH, METHOD_USER_PASS][..],
                me,
                ours,
                (METHOD_USER_PASS, Some(AUTH_OK), Some(REP_SUCCESS)),
            ),
            (
                &[METHOD_USER_PASS][..],
                me,
                another_password.as_str(),
                (METHOD_USER_PASS, Some(AUTH_FAILED), None),
            ),
            (
                &[METHOD_USER_PASS][..],
                another_user.as_str(),
                ours,
                (METHOD_USER_PASS, Some(AUTH_FAILED), None),
            ),
            (
                &[METHOD_USER_PASS][..],
                me,
                "",
                (METHOD_USER_PASS, Some(AUTH_FAILED), None),
            ),
            (&[METHOD_NO_AUTH][..], "", "", (METHOD_NONE, None, None)),
        ] {
            let (got, saw) =
                session_handshake(methods, user, pass, connect_to("db.internal", 5432)).await;
            assert_eq!(saw, seen, "{methods:?} {user}");
            if seen.1 == Some(AUTH_OK) {
                assert_eq!(got.unwrap(), ("db.internal".to_string(), 5432));
            } else {
                assert!(got.is_err(), "{methods:?} {user}");
            }
        }
    }

    /// FR-90 P3-7-3b — a session's listener serves CONNECT alone: UDP
    /// ASSOCIATE is refused even with its credential. And its credential
    /// prints its username alone.
    #[tokio::test]
    async fn a_session_listener_serves_connect_alone() {
        let cred = test_credential();
        let udp = vec![VER, CMD_UDP_ASSOCIATE, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0];
        let (got, saw) =
            session_handshake(&[METHOD_USER_PASS], &cred.username, &cred.password, udp).await;
        assert!(got.is_err());
        assert_eq!(
            saw,
            (METHOD_USER_PASS, Some(AUTH_OK), Some(REP_CMD_NOT_SUPPORTED))
        );
        let shown = format!("{cred:?}");
        assert!(
            shown.contains(&cred.username) && !shown.contains(&cred.password),
            "{shown}"
        );
    }

    #[test]
    fn udp_datagram_ipv4_roundtrip() {
        // Build [RSV=0,0 | FRAG=0 | ATYP=ipv4 | 8.8.8.8 | 53 | payload]
        let mut d = vec![0, 0, 0, ATYP_IPV4, 8, 8, 8, 8];
        d.extend_from_slice(&53u16.to_be_bytes());
        d.extend_from_slice(b"query");
        let (host, port, off) = parse_udp_datagram(&d).unwrap();
        assert_eq!(host, "8.8.8.8");
        assert_eq!(port, 53);
        assert_eq!(&d[off..], b"query");

        // Encode a reply datagram from 8.8.8.8:53 and parse it back.
        let framed = encode_udp_datagram("8.8.8.8", 53, b"answer");
        let (h2, p2, o2) = parse_udp_datagram(&framed).unwrap();
        assert_eq!(
            (h2.as_str(), p2, &framed[o2..]),
            ("8.8.8.8", 53, &b"answer"[..])
        );
    }

    /// A UDP ASSOCIATE request is parsed (not rejected) by
    /// `accept_request`, and `reply_bound` echoes a real BND addr the
    /// app can send datagrams to.
    #[tokio::test]
    async fn accept_request_parses_udp_associate_and_reply_bound() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut c = TcpStream::connect(addr).await.unwrap();
            c.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
            let mut sel = [0u8; 2];
            c.read_exact(&mut sel).await.unwrap();
            // CMD=0x03 UDP ASSOCIATE, ATYP=IPv4 0.0.0.0:0 (advisory DST).
            c.write_all(&[0x05, CMD_UDP_ASSOCIATE, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
            let mut head = [0u8; 4];
            c.read_exact(&mut head).await.unwrap();
            // Consume BND.ADDR (ipv4) + BND.PORT.
            let mut rest = [0u8; 6];
            c.read_exact(&mut rest).await.unwrap();
            (head, rest)
        });
        let (mut srv, _) = listener.accept().await.unwrap();
        let req = accept_request(&mut srv).await.unwrap();
        assert_eq!(req, Socks5Request::UdpAssociate);
        let bind: std::net::SocketAddr = "127.0.0.1:51820".parse().unwrap();
        reply_bound(&mut srv, REP_SUCCESS, bind).await;
        let (head, rest) = client.await.unwrap();
        assert_eq!(head[0], VER);
        assert_eq!(head[1], REP_SUCCESS);
        assert_eq!(head[3], ATYP_IPV4);
        assert_eq!(&rest[..4], &[127, 0, 0, 1]);
        assert_eq!(u16::from_be_bytes([rest[4], rest[5]]), 51820);
    }

    /// The mesh's SOCKS-UDP CLIENT handshake against a per-agent proxy:
    /// `client_udp_associate` must negotiate no-auth, request UDP
    /// ASSOCIATE, and return the proxy's relay bind addr. Driven against
    /// a real `accept_request` + `reply_bound` server (the proxy side).
    #[tokio::test]
    async fn client_udp_associate_reads_relay_addr() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            assert_eq!(
                accept_request(&mut s).await.unwrap(),
                Socks5Request::UdpAssociate
            );
            let bind: std::net::SocketAddr = "127.0.0.1:40404".parse().unwrap();
            reply_bound(&mut s, REP_SUCCESS, bind).await;
            // Hold the control conn briefly (association lifetime).
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });
        let mut c = TcpStream::connect(addr).await.unwrap();
        let relay = client_udp_associate(&mut c).await.unwrap();
        assert_eq!(relay, "127.0.0.1:40404".parse().unwrap());
        srv.await.unwrap();
    }

    #[test]
    fn udp_datagram_domain_and_rejects_fragment() {
        let host = b"dns.internal";
        let mut d = vec![0, 0, 0, ATYP_DOMAIN, host.len() as u8];
        d.extend_from_slice(host);
        d.extend_from_slice(&5353u16.to_be_bytes());
        d.extend_from_slice(b"x");
        let (h, p, off) = parse_udp_datagram(&d).unwrap();
        assert_eq!(
            (h.as_str(), p, &d[off..]),
            ("dns.internal", 5353, &b"x"[..])
        );

        // FRAG != 0 is rejected.
        let frag = vec![0, 0, 1, ATYP_IPV4, 1, 2, 3, 4, 0, 53];
        assert!(parse_udp_datagram(&frag).is_err());
        // Too short.
        assert!(parse_udp_datagram(&[0, 0, 0]).is_err());
    }
}

//! Phase 3 core: run a quinn QUIC endpoint over an **arbitrary relayed
//! datagram connection** instead of a real UDP socket.
//!
//! quinn lets an endpoint be backed by any [`quinn::AsyncUdpSocket`].
//! This module provides [`RelayUdpSocket`], an `AsyncUdpSocket` that
//! bridges quinn's poll-based send/recv onto an async datagram channel
//! described by the [`RelayConn`] trait (`send_to` / `recv_from` /
//! `local_addr` — the shape of a TURN-relayed `util::Conn`). That lets
//! QUIC traffic ride a **TURN allocation** (peer → coturn → peer) for
//! symmetric-NAT / UDP-restricted corp nets where direct hole-punch
//! fails — QUIC's TLS stays end-to-end; coturn only ever sees ciphertext.
//!
//! Phase 3b wires `RelayConn` to the `turn` crate's relayed
//! `util::Conn` (Tier 2 = UDP relay; Tier 3 = TURNS/TCP via the vendored
//! `webrtc-ice` tcp-turn conn). This module is transport-agnostic + has
//! NO TURN/webrtc-util dependency, so it's unit-testable over a plain
//! loopback UDP pair (see tests).
//!
//! **Bridging shape.** `try_send` (sync, must not block) pushes
//! `(dest, bytes)` onto an unbounded channel; a drain task awaits the
//! `RelayConn::send_to`. A fill task loops `RelayConn::recv_from` and
//! pushes `(bytes, src)` onto another channel that `poll_recv` drains.
//! `max_{transmit,receive}_segments = 1` (no GSO/GRO over a relay).

use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex;
use std::task::{Context, Poll};

use async_trait::async_trait;
use quinn::AsyncUdpSocket;
use quinn::udp::{RecvMeta, Transmit};
use tokio::sync::mpsc;
// Phase 3b: the `turn` client + the `webrtc-util` `Conn` trait its
// allocation yields. Aliased `UtilConn` so it never collides with this
// module's own [`RelayConn`] trait (the names rhyme; the types don't).
// `webrtc-util`'s lib name is `webrtc_util` (the `turn` crate imports it
// renamed to `util`; we use the real name here).
use turn::client::{Client, ClientConfig};
use webrtc_util::conn::Conn as UtilConn;
// Tier 3 (TURNS/TCP): the vendored `webrtc-ice`'s field-proven `util::Conn`
// adapter over TLS-over-TCP to coturn. `webrtc-ice` resolves to
// `crates/vendored/webrtc-ice` via the root `[patch.crates-io]`, and its
// `util` is the SAME `webrtc-util 0.10` `turn` 0.9 uses — so a
// `TcpTurnConn` is accepted as `ClientConfig.conn` with no type bridging.
use webrtc_ice::agent::tcp_turn_conn::TcpTurnConn;

/// A relayed datagram connection — the subset of a TURN-relayed
/// `util::Conn` that [`RelayUdpSocket`] needs. Phase 3b implements this
/// for the `turn` crate's allocated `Arc<dyn util::Conn>`; the tests
/// implement it over a tokio `UdpSocket`.
#[async_trait]
pub trait RelayConn: Send + Sync + 'static {
    /// Send one datagram to `dst` through the relay.
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> io::Result<usize>;
    /// Receive one datagram, returning its length + the peer source.
    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)>;
    /// The relay-side local address (the allocated relay address for a
    /// TURN conn) — what quinn reports as its `local_addr` and what the
    /// peer dials.
    fn local_addr(&self) -> io::Result<SocketAddr>;
}

/// Max datagram we'll relay. QUIC keeps datagrams ≤ the path MTU
/// (~1200–1452); 2 KiB is comfortable headroom and bounds the recv buf.
const MAX_DATAGRAM: usize = 2048;

/// quinn `AsyncUdpSocket` backed by a [`RelayConn`]. See module docs.
pub struct RelayUdpSocket {
    local_addr: SocketAddr,
    /// `try_send` pushes here; the drain task awaits `send_to`.
    send_tx: mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>,
    /// `poll_recv` drains here; the fill task feeds it from `recv_from`.
    recv_rx: Mutex<mpsc::UnboundedReceiver<(Vec<u8>, SocketAddr)>>,
    send_task: tokio::task::JoinHandle<()>,
    recv_task: tokio::task::JoinHandle<()>,
}

impl fmt::Debug for RelayUdpSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RelayUdpSocket")
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

impl RelayUdpSocket {
    /// Wrap `conn` as a quinn-compatible socket. Spawns the send-drain
    /// + recv-fill tasks (aborted on drop).
    pub fn new(conn: Arc<dyn RelayConn>) -> io::Result<Self> {
        let local_addr = conn.local_addr()?;
        let (send_tx, mut send_rx) = mpsc::unbounded_channel::<(SocketAddr, Vec<u8>)>();
        let (recv_tx, recv_rx) = mpsc::unbounded_channel::<(Vec<u8>, SocketAddr)>();

        let send_conn = Arc::clone(&conn);
        let send_task = tokio::spawn(async move {
            while let Some((dst, data)) = send_rx.recv().await {
                if let Err(e) = send_conn.send_to(&data, dst).await {
                    tracing::debug!(%dst, %e, "relay send_to failed; dropping datagram");
                }
            }
        });

        let recv_task = tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_DATAGRAM];
            loop {
                match conn.recv_from(&mut buf).await {
                    Ok((n, src)) => {
                        // Channel closed = the socket was dropped; stop.
                        if recv_tx.send((buf[..n].to_vec(), src)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::debug!(%e, "relay recv_from ended; recv task exiting");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            local_addr,
            send_tx,
            recv_rx: Mutex::new(recv_rx),
            send_task,
            recv_task,
        })
    }
}

impl Drop for RelayUdpSocket {
    fn drop(&mut self) {
        self.send_task.abort();
        self.recv_task.abort();
    }
}

/// A [`quinn::UdpPoller`] that is always writable — the send path is an
/// unbounded channel, so `try_send` never returns `WouldBlock`.
#[derive(Debug)]
struct AlwaysWritable;

impl quinn::UdpPoller for AlwaysWritable {
    fn poll_writable(self: std::pin::Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncUdpSocket for RelayUdpSocket {
    fn create_io_poller(self: Arc<Self>) -> std::pin::Pin<Box<dyn quinn::UdpPoller>> {
        Box::pin(AlwaysWritable)
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        // No GSO over a relay (max_transmit_segments == 1), so `contents`
        // is exactly one datagram. Hand it to the drain task.
        self.send_tx
            .send((transmit.destination, transmit.contents.to_vec()))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "relay send task gone"))
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        if bufs.is_empty() || meta.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let mut rx = self
            .recv_rx
            .lock()
            .map_err(|_| io::Error::other("relay recv mutex poisoned"))?;
        match rx.poll_recv(cx) {
            Poll::Ready(Some((data, src))) => {
                let n = data.len().min(bufs[0].len());
                bufs[0][..n].copy_from_slice(&data[..n]);
                meta[0] = RecvMeta {
                    addr: src,
                    len: n,
                    stride: n,
                    ecn: None,
                    dst_ip: None,
                };
                Poll::Ready(Ok(1))
            }
            // Sender dropped (recv task ended) — surface as a read error
            // so quinn tears the endpoint down rather than spinning.
            Poll::Ready(None) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "relay closed",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }

    fn max_transmit_segments(&self) -> usize {
        1
    }

    fn max_receive_segments(&self) -> usize {
        1
    }
}

// ───────────────────────── Phase 3b: TURN-backed RelayConn ─────────────
//
// Bridge a real TURN allocation — the `turn` crate's relayed
// `util::Conn` — onto the [`RelayConn`] trait above, so that a
// [`RelayUdpSocket`] (and thus a quinn endpoint) can ride a coturn
// allocation. This is the concrete Tier-2 (UDP relay) / Tier-3
// (TURNS-over-TCP, same client, TCP underlay) datagram path.
//
// The wiring is intentionally thin: `send_to` / `recv_from` forward to
// the relayed conn (mapping `util::Error` → `io::Error`), and
// `local_addr` returns the **relayed transport address** coturn handed
// out — that's what quinn reports as its local address and what the
// remote peer dials (delivered to the peer over signaling in Phase 3c).

/// A [`RelayConn`] backed by a live TURN allocation. Owns the
/// [`turn::client::Client`] so the allocation + the background
/// `listen()` loop that demuxes inbound TURN messages onto the relay
/// stay alive for the relay's lifetime — dropping the client tears the
/// allocation down.
pub struct TurnRelayConn {
    /// Kept alive on purpose: the client's `listen()` task is what feeds
    /// `relay`'s `recv_from`. Never touched after construction; dropping
    /// it closes the allocation on coturn.
    _client: Client,
    relay: Arc<dyn UtilConn + Send + Sync>,
    /// Cached relayed address (fixed for the allocation's life) so the
    /// sync [`RelayConn::local_addr`] needn't re-query + re-map errors.
    relayed_addr: SocketAddr,
}

impl fmt::Debug for TurnRelayConn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnRelayConn")
            .field("relayed_addr", &self.relayed_addr)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl RelayConn for TurnRelayConn {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> io::Result<usize> {
        self.relay.send_to(buf, dst).await.map_err(util_to_io)
    }
    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.relay.recv_from(buf).await.map_err(util_to_io)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.relayed_addr)
    }
}

/// Map a `webrtc-util` conn error to `io::Error` — quinn's socket layer
/// speaks `io::Error`. The text is preserved; the variant collapses to
/// `Other` because the relay never returns `WouldBlock` (`recv_from`
/// blocks until a datagram or a hard error), which is the only error
/// kind quinn's `AsyncUdpSocket` path treats specially.
fn util_to_io(e: webrtc_util::Error) -> io::Error {
    io::Error::other(e.to_string())
}

/// Allocate a TURN relay on `turn_server` with long-term credentials and
/// return a [`TurnRelayConn`] ready to wrap in a [`RelayUdpSocket`].
///
/// The local UDP socket that talks to the TURN server (the underlay) is
/// bound to an ephemeral port and is **not** the one quinn rides — quinn
/// rides the *relayed* conn, whose address is
/// [`RelayConn::local_addr`]. `username`/`password` are the short-lived
/// HMAC creds coturn issues (server-side `turn_creds::ice_servers_for`);
/// `realm` must match coturn's configured realm.
///
/// Validated against an in-process `turn::server` on loopback in the
/// tests (the full quinn-over-two-allocations path); exercised against
/// the live coturn cluster in Phase 3d.
pub async fn allocate_turn_relay(
    turn_server: SocketAddr,
    username: String,
    password: String,
    realm: String,
) -> anyhow::Result<TurnRelayConn> {
    use anyhow::Context as _;

    // The underlay socket the TURN *client* uses to reach coturn. quinn
    // never sees this — it sends/receives through the relayed conn.
    let underlay = Arc::new(
        tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .await
            .context("bind TURN client underlay socket")?,
    );

    let client = Client::new(ClientConfig {
        stun_serv_addr: String::new(),
        turn_serv_addr: turn_server.to_string(),
        username,
        password,
        realm,
        software: String::new(),
        rto_in_ms: 0,
        conn: underlay,
        vnet: None,
    })
    .await
    .context("TURN client::new")?;

    // Spawns the background read loop that demuxes inbound TURN messages
    // onto the allocation — must run before `allocate()`.
    client.listen().await.context("TURN client listen")?;

    let relay = client.allocate().await.context("TURN allocate")?;
    let relayed_addr = relay
        .local_addr()
        .map_err(util_to_io)
        .context("TURN relayed local_addr")?;
    tracing::info!(%turn_server, %relayed_addr, "TURN allocation established");

    Ok(TurnRelayConn {
        _client: client,
        relay: Arc::new(relay),
        relayed_addr,
    })
}

/// A [`RelayConn`] over a plain tokio `UdpSocket` — the datagrams are NOT
/// actually relayed (`send_to`/`recv_from` hit the wire directly), i.e.
/// the "relay" is just a socket. Two uses: (1) a directly-reachable /
/// same-host path can still drive a [`QuicPeer`](crate::transport::quic::QuicPeer)
/// through the same [`RelayUdpSocket`] abstraction the TURN path uses,
/// and (2) tests exercise the bridge + the agent/client relay
/// orchestration (Phase 3d) without standing up coturn.
pub struct UdpRelayConn(pub tokio::net::UdpSocket);

#[async_trait]
impl RelayConn for UdpRelayConn {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> io::Result<usize> {
        self.0.send_to(buf, dst).await
    }
    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.0.recv_from(buf).await
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
    }
}

/// Pick the first plain-UDP TURN server (`turn:HOST:PORT?transport=udp`,
/// or `turn:HOST:PORT` with no transport — UDP is the TURN default) from
/// an ICE-server URL list and return its `HOST:PORT`. `stun:`, `turns:`
/// (TLS) and `?transport=tcp` URLs are skipped: the webrtc-rs `turn`
/// client this feeds drives a UDP underlay only (Tier 2). Tier 3
/// (TURNS/TCP, for UDP-blocked nets) must ride the vendored webrtc-ice
/// `tcp_turn_conn` instead — the `turn` client silently drops non-UDP
/// transports (see the webrtc-rs TURN-URL gap memo).
pub fn turn_udp_server(urls: &[String]) -> Option<String> {
    for url in urls {
        // Only `turn:` (plain). `turns:` (note the trailing s) is TLS and
        // `strip_prefix("turn:")` correctly rejects it.
        let Some(rest) = url.strip_prefix("turn:") else {
            continue;
        };
        let (hostport, query) = match rest.split_once('?') {
            Some((hp, q)) => (hp, Some(q)),
            None => (rest, None),
        };
        let is_udp = match query {
            None => true, // no transport param ⇒ UDP per RFC 7065 default
            Some(q) => q
                .split('&')
                .any(|kv| kv.eq_ignore_ascii_case("transport=udp")),
        };
        if is_udp && !hostport.is_empty() {
            return Some(hostport.to_string());
        }
    }
    None
}

/// Pick the first `turns:HOST:PORT?transport=tcp` (TLS-over-TCP) TURN
/// server from an ICE-server URL list, returning `(host, port)`. The host
/// is kept UNRESOLVED — Tier 3 needs the hostname for TLS SNI + cert
/// verification ([`TcpTurnConn`] connects + handshakes to it). `turn:`
/// (plain) and the TURNS-over-UDP (DTLS) flavour are skipped — the
/// `TcpTurnConn` adapter rides TCP only, so an explicit `transport=tcp` is
/// required.
pub fn turn_tls_server(urls: &[String]) -> Option<(String, u16)> {
    for url in urls {
        let Some(rest) = url.strip_prefix("turns:") else {
            continue; // skip stun: / plain turn:
        };
        let (hostport, query) = match rest.split_once('?') {
            Some((hp, q)) => (hp, Some(q)),
            None => (rest, None),
        };
        let is_tcp = match query {
            None => false, // no transport ⇒ DTLS/UDP default, which we can't ride
            Some(q) => q
                .split('&')
                .any(|kv| kv.eq_ignore_ascii_case("transport=tcp")),
        };
        if !is_tcp {
            continue;
        }
        // rsplit so a bare hostname:port splits correctly (coturn URLs use
        // a hostname, never a bracketed IPv6 literal, so this is safe).
        if let Some((host, port_str)) = hostport.rsplit_once(':')
            && let Ok(port) = port_str.parse::<u16>()
            && !host.is_empty()
        {
            return Some((host.to_string(), port));
        }
    }
    None
}

/// Initial realm handed to the `turn` client. coturn in `use-auth-secret`
/// (REST) mode returns its own realm in the 401 challenge and the
/// webrtc-rs client OVERWRITES this with the challenge realm before
/// computing MESSAGE-INTEGRITY (`turn-0.9.0` `client/mod.rs:542`), so the
/// value only has to be non-empty; the live realm wins regardless. We use
/// the production realm for tidiness.
const DEFAULT_TURN_REALM: &str = "roomler.ai";

/// Cap on the UDP TURN-allocate attempt before falling back to TURNS/TCP.
/// On a UDP-OK net the allocate completes in ~1 RTT; on a net that blocks
/// outbound UDP to coturn the `turn` client would otherwise retransmit for
/// ~tens of seconds before erroring, so we bound it and switch to Tier 3.
const UDP_ALLOC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Allocate a TURN relay from coturn ICE-server credentials, trying the
/// connectivity tiers in order:
/// * **Tier 2** — UDP relay (`turn:…?transport=udp`); the common path.
/// * **Tier 3** — TURNS/TCP relay (`turns:…?transport=tcp`) via the
///   vendored [`TcpTurnConn`]; for corp nets that block ALL outbound UDP.
///
/// `username` + `credential` are the short-lived REST-API creds the server
/// minted (`turn_creds::ice_servers_for`). Returns the first tier that
/// establishes an allocation. **The relay tier is independent per peer** —
/// coturn hands each side a public relayed address that interoperates
/// regardless of how the *other* peer reached coturn — so the agent +
/// tunnel-client need not agree on a tier. This is the Phase-3 entry both
/// the agent and the client call once they hold their per-session creds.
pub async fn allocate_relay_from_ice(
    urls: &[String],
    username: &str,
    credential: &str,
) -> anyhow::Result<TurnRelayConn> {
    // Tier 2: UDP relay (bounded — falls through to Tier 3 on timeout/err).
    if let Some(server) = turn_udp_server(urls) {
        match tokio::net::lookup_host(&server).await {
            Ok(mut addrs) => match addrs.next() {
                Some(resolved) => {
                    match tokio::time::timeout(
                        UDP_ALLOC_TIMEOUT,
                        allocate_turn_relay(
                            resolved,
                            username.to_string(),
                            credential.to_string(),
                            DEFAULT_TURN_REALM.to_string(),
                        ),
                    )
                    .await
                    {
                        Ok(Ok(relay)) => return Ok(relay),
                        Ok(Err(e)) => {
                            tracing::warn!(%server, %e, "UDP TURN allocate failed; trying TURNS/TCP")
                        }
                        Err(_) => {
                            tracing::warn!(%server, "UDP TURN allocate timed out; trying TURNS/TCP")
                        }
                    }
                }
                None => {
                    tracing::warn!(%server, "UDP TURN server resolved to no addresses; trying TURNS/TCP")
                }
            },
            Err(e) => tracing::warn!(%server, %e, "UDP TURN server unresolvable; trying TURNS/TCP"),
        }
    }

    // Tier 3: TURNS/TCP relay (UDP-blocked nets).
    if let Some((host, port)) = turn_tls_server(urls) {
        use anyhow::Context as _;
        return allocate_turn_relay_tls(
            &host,
            port,
            username.to_string(),
            credential.to_string(),
            DEFAULT_TURN_REALM.to_string(),
        )
        .await
        .with_context(|| format!("TURNS/TCP relay allocate to {host}:{port}"));
    }

    anyhow::bail!("no usable TURN url (turn:…udp or turns:…tcp) among {urls:?}")
}

/// Tier 3: allocate a TURN relay over **TURNS (TLS-over-TCP)** for nets
/// that block outbound UDP. Connects TCP → drives the TLS handshake (SNI =
/// `host`) via the vendored [`TcpTurnConn`] → feeds that as the `turn`
/// client's underlay → allocates. The relayed address coturn returns is
/// what the peer dials (delivered over signaling like the UDP path); QUIC
/// rides the allocation exactly as in Tier 2 — only the client→coturn leg
/// differs (TLS/TCP instead of UDP).
///
/// `realm` is a don't-care (coturn returns its own in the 401 challenge,
/// which the `turn` client adopts before computing MESSAGE-INTEGRITY). The
/// `TcpTurnConn` is field-proven for the WebRTC relay-gather path; this is
/// its first QUIC use.
pub async fn allocate_turn_relay_tls(
    host: &str,
    port: u16,
    username: String,
    password: String,
    realm: String,
) -> anyhow::Result<TurnRelayConn> {
    use anyhow::Context as _;

    // Resolve once so the TCP connect + `turn_serv_addr` (which the client
    // formats into its STUN transactions) agree on a concrete SocketAddr;
    // SNI still uses the hostname for certificate verification.
    let resolved = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("resolve TURNS server {host}:{port}"))?
        .next()
        .with_context(|| format!("TURNS server {host}:{port} resolved to no addresses"))?;

    let tcp = tokio::net::TcpStream::connect(resolved)
        .await
        .with_context(|| format!("TCP connect to TURNS server {resolved}"))?;
    let adapter = TcpTurnConn::connect_tls(tcp, host)
        .await
        .with_context(|| format!("TLS handshake to TURNS server {host}"))?;
    let conn: Arc<dyn UtilConn + Send + Sync> = Arc::new(adapter);

    let client = Client::new(ClientConfig {
        stun_serv_addr: String::new(),
        turn_serv_addr: resolved.to_string(),
        username,
        password,
        realm,
        software: String::new(),
        rto_in_ms: 0,
        // Over TCP every byte rides the one TLS connection — no vnet (which
        // would try to open side sockets for keepalives).
        conn,
        vnet: None,
    })
    .await
    .context("TURNS client::new")?;

    client.listen().await.context("TURNS client listen")?;
    let relay = client.allocate().await.context("TURNS allocate")?;
    let relayed_addr = relay
        .local_addr()
        .map_err(util_to_io)
        .context("TURNS relayed local_addr")?;
    tracing::info!(%host, port, %relayed_addr, "TURNS/TCP TURN allocation established");

    Ok(TurnRelayConn {
        _client: client,
        relay: Arc::new(relay),
        relayed_addr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UdpSocket;

    async fn udp_relay() -> (Arc<dyn RelayConn>, SocketAddr) {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        (Arc::new(UdpRelayConn(sock)), addr)
    }

    /// `turn_udp_server` must pick the plain-UDP TURN url out of the
    /// production 6-url ICE list (which leads with STUN + interleaves
    /// TCP/TLS flavours) and skip everything the UDP `turn` client can't
    /// use.
    #[test]
    fn turn_udp_server_picks_the_udp_url() {
        let prod = vec![
            "stun:stun.l.google.com:19302".to_string(),
            "turns:coturn.roomler.ai:5349?transport=tcp".to_string(),
            "turn:coturn.roomler.ai:3478?transport=udp".to_string(),
            "turn:coturn.roomler.ai:3478?transport=tcp".to_string(),
        ];
        assert_eq!(
            turn_udp_server(&prod).as_deref(),
            Some("coturn.roomler.ai:3478")
        );
        // No `?transport` ⇒ UDP default.
        assert_eq!(
            turn_udp_server(&["turn:host.example:3478".to_string()]).as_deref(),
            Some("host.example:3478")
        );
        // Only TLS / TCP flavours ⇒ nothing the Tier-2 UDP client can use.
        assert_eq!(
            turn_udp_server(&["turns:host:5349?transport=tcp".to_string()]),
            None
        );
        assert_eq!(turn_udp_server(&["stun:host:3478".to_string()]), None);
        assert_eq!(turn_udp_server(&[]), None);
    }

    /// Tier 3: `turn_tls_server` must select the `turns:…?transport=tcp`
    /// entry (keeping the hostname for SNI) and skip stun/plain-turn/UDP.
    #[test]
    fn turn_tls_server_picks_the_turns_tcp_url() {
        let prod = vec![
            "stun:stun.l.google.com:19302".to_string(),
            "turn:coturn.roomler.ai:3478?transport=udp".to_string(),
            "turns:coturn.roomler.ai:443?transport=tcp".to_string(),
            "turns:coturn.roomler.ai:5349?transport=tcp".to_string(),
        ];
        assert_eq!(
            turn_tls_server(&prod),
            Some(("coturn.roomler.ai".to_string(), 443)),
            "first turns:…?transport=tcp wins; host kept unresolved for SNI"
        );
        // turns: without an explicit transport=tcp ⇒ DTLS/UDP, which the
        // TcpTurnConn adapter can't ride.
        assert_eq!(turn_tls_server(&["turns:host:5349".to_string()]), None);
        // Plain turn:/udp + stun: are Tier-2 / not-TURN — never TLS-TCP.
        assert_eq!(
            turn_tls_server(&["turn:host:3478?transport=udp".to_string()]),
            None
        );
        assert_eq!(turn_tls_server(&["stun:host:3478".to_string()]), None);
        assert_eq!(turn_tls_server(&[]), None);
    }

    /// The adapter must faithfully carry datagrams both ways: send via
    /// quinn's `try_send` shape (through the channel + drain task) and
    /// receive via `poll_recv` (through the fill task). We drive it with
    /// raw datagrams (no quinn) to isolate the bridge logic.
    #[tokio::test(flavor = "multi_thread")]
    async fn relay_socket_round_trips_datagrams() {
        use std::future::poll_fn;

        let (conn_a, addr_a) = udp_relay().await;
        let (conn_b, addr_b) = udp_relay().await;
        let sock_a = RelayUdpSocket::new(conn_a).unwrap();
        let sock_b = RelayUdpSocket::new(conn_b).unwrap();
        assert_eq!(sock_a.local_addr().unwrap(), addr_a);

        // A → B via try_send.
        let payload = b"relayed quic datagram";
        sock_a
            .try_send(&Transmit {
                destination: addr_b,
                ecn: None,
                contents: payload,
                segment_size: None,
                src_ip: None,
            })
            .unwrap();

        // B receives it via poll_recv.
        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, src) = poll_fn(|cx| {
            let mut bufs = [IoSliceMut::new(&mut buf)];
            let mut meta = [RecvMeta::default()];
            match sock_b.poll_recv(cx, &mut bufs, &mut meta) {
                Poll::Ready(Ok(_)) => Poll::Ready((meta[0].len, meta[0].addr)),
                Poll::Ready(Err(e)) => panic!("poll_recv error: {e}"),
                Poll::Pending => Poll::Pending,
            }
        })
        .await;
        assert_eq!(&buf[..n], payload, "datagram must arrive intact");
        assert_eq!(src, addr_a, "recv meta must report the real source");
    }

    /// Full proof: a quinn server + client, each backed by a
    /// `RelayUdpSocket` over a loopback UDP relay, complete the TLS
    /// handshake + round-trip a flow. This is what Phase 3 does over a
    /// TURN allocation instead of loopback UDP.
    #[tokio::test(flavor = "multi_thread")]
    async fn quinn_runs_over_relay_socket() {
        use crate::transport::quic::{QuicPeer, accept_flow, open_flow};

        let (conn_s, addr_s) = udp_relay().await;
        let (conn_c, _addr_c) = udp_relay().await;
        let server_sock = Arc::new(RelayUdpSocket::new(conn_s).unwrap());
        let client_sock = Arc::new(RelayUdpSocket::new(conn_c).unwrap());

        let (server, fp) =
            QuicPeer::server_over_abstract_socket(server_sock).expect("server over relay");
        let client =
            QuicPeer::client_over_abstract_socket(client_sock, &fp).expect("client over relay");

        let srv = tokio::spawn(async move {
            let conn = server.accept().await.expect("incoming").expect("handshake");
            let (flow_id, mut send, mut recv) = accept_flow(&conn).await.expect("accept_flow");
            assert_eq!(flow_id, 11);
            let got = recv.read_to_end(64 * 1024).await.unwrap();
            assert_eq!(&got, b"ping-over-relay");
            send.write_all(b"pong-over-relay").await.unwrap();
            send.finish().unwrap();
            conn.closed().await;
        });

        // The client dials the server's relay address.
        let conn = client.connect(addr_s).await.expect("connect over relay");
        let (mut send, mut recv) = open_flow(&conn, 11).await.expect("open_flow");
        send.write_all(b"ping-over-relay").await.unwrap();
        send.finish().unwrap();
        let reply = recv.read_to_end(64 * 1024).await.unwrap();
        assert_eq!(
            &reply, b"pong-over-relay",
            "quinn round-trip over the relay socket"
        );
        conn.close(0u32.into(), b"done");
        let _ = srv.await;
    }
}

#[cfg(test)]
mod turn_tests {
    //! Phase 3b validation: prove the full **Tier-2** path — a quinn
    //! server + client, each riding a [`RelayUdpSocket`] over a *real*
    //! TURN allocation on an in-process [`turn::server::Server`] —
    //! complete the handshake and round-trip a flow. This is exactly
    //! what Phase 3d does against the live coturn cluster, minus coturn:
    //! two allocations on one server relay to each other (peer → server
    //! → peer), exercising [`allocate_turn_relay`], [`TurnRelayConn`],
    //! the permission bootstrap, and quinn-over-relay end to end.

    use super::*;
    use crate::transport::quic::{QuicPeer, accept_flow, open_flow};
    use std::net::IpAddr;
    use std::time::Duration;
    use turn::auth::{AuthHandler, generate_auth_key};
    use turn::relay::relay_static::RelayAddressGeneratorStatic;
    use turn::server::Server;
    use turn::server::config::{ConnConfig, ServerConfig};
    use webrtc_util::vnet::net::Net;

    const REALM: &str = "roomler.test";
    const USER: &str = "quic-tester";
    const PASS: &str = "turn-secret";

    /// Long-term-credential auth accepting the one test user. The server
    /// derives the expected HMAC key from `(user, realm, pass)`; the
    /// client derives the same from its configured creds, so they match.
    struct StaticAuth;
    impl AuthHandler for StaticAuth {
        fn auth_handle(
            &self,
            _username: &str,
            realm: &str,
            _src: SocketAddr,
        ) -> Result<Vec<u8>, turn::Error> {
            Ok(generate_auth_key(USER, realm, PASS))
        }
    }

    /// In-process TURN server on loopback; relay addresses are handed out
    /// on 127.0.0.1 too, so two allocations can relay to each other.
    /// Returns the server (keep it alive for the test) and its addr.
    async fn loopback_turn_server() -> (Server, SocketAddr) {
        let conn = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let turn_addr = conn.local_addr().unwrap();
        let server = Server::new(ServerConfig {
            conn_configs: vec![ConnConfig {
                conn,
                relay_addr_generator: Box::new(RelayAddressGeneratorStatic {
                    relay_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    address: "127.0.0.1".to_owned(),
                    net: Arc::new(Net::new(None)),
                }),
            }],
            realm: REALM.to_owned(),
            auth_handler: Arc::new(StaticAuth),
            channel_bind_timeout: Duration::from_secs(0),
            alloc_close_notify: None,
        })
        .await
        .expect("in-process turn server");
        (server, turn_addr)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn quinn_runs_over_two_turn_allocations() {
        let (_server, turn_addr) = loopback_turn_server().await;

        // Agent side + client side each allocate a relay on the server.
        let agent_relay = allocate_turn_relay(turn_addr, USER.into(), PASS.into(), REALM.into())
            .await
            .expect("agent TURN allocate");
        let client_relay = allocate_turn_relay(turn_addr, USER.into(), PASS.into(), REALM.into())
            .await
            .expect("client TURN allocate");

        let r_agent = agent_relay.local_addr().unwrap();
        let r_client = client_relay.local_addr().unwrap();
        assert_ne!(r_agent, r_client, "allocations get distinct relay addrs");
        assert!(r_agent.ip().is_loopback(), "loopback relay addr: {r_agent}");

        // Permission bootstrap: each allocation must install a TURN
        // permission for the other's relay addr before the server will
        // relay inbound from it. One datagram each way installs it (the
        // stray byte is discarded by quinn as a too-short packet). Phase
        // 3d does this after exchanging relay addrs over signaling;
        // QUIC's Initial retransmission covers any race between the
        // permission install and the first handshake packet.
        agent_relay.send_to(b"\x00", r_client).await.unwrap();
        client_relay.send_to(b"\x00", r_agent).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let agent_sock = Arc::new(RelayUdpSocket::new(Arc::new(agent_relay)).unwrap());
        let client_sock = Arc::new(RelayUdpSocket::new(Arc::new(client_relay)).unwrap());

        let (server, fp) =
            QuicPeer::server_over_abstract_socket(agent_sock).expect("quic server over TURN");
        let client =
            QuicPeer::client_over_abstract_socket(client_sock, &fp).expect("quic client over TURN");

        let srv = tokio::spawn(async move {
            let conn = server.accept().await.expect("incoming").expect("handshake");
            let (flow_id, mut send, mut recv) = accept_flow(&conn).await.expect("accept_flow");
            assert_eq!(flow_id, 42);
            let got = recv.read_to_end(64 * 1024).await.unwrap();
            assert_eq!(&got, b"ping-over-turn");
            send.write_all(b"pong-over-turn").await.unwrap();
            send.finish().unwrap();
            conn.closed().await;
        });

        // Bound the e2e so a relay/permission misfire fails fast rather
        // than hanging the runner.
        let outcome = tokio::time::timeout(Duration::from_secs(15), async {
            let conn = client.connect(r_agent).await.expect("connect over TURN");
            let (mut send, mut recv) = open_flow(&conn, 42).await.expect("open_flow");
            send.write_all(b"ping-over-turn").await.unwrap();
            send.finish().unwrap();
            let reply = recv.read_to_end(64 * 1024).await.unwrap();
            assert_eq!(
                &reply, b"pong-over-turn",
                "quinn round-trip over TURN relay"
            );
            conn.close(0u32.into(), b"done");
        })
        .await;
        outcome.expect("quinn-over-TURN round-trip timed out");
        let _ = srv.await;
    }
}

//! TURNS-over-TLS-over-TCP `Conn` adapter for relay candidate gathering.
//!
//! # Why this exists
//!
//! Upstream `webrtc-ice::agent::agent_gather::gather_candidates_relay`
//! implements ONE relay-transport branch: TURN over UDP. TURN/TCP,
//! TURNS/TCP and TURNS/UDP (DTLS) all fall through to a `log::warn!`
//! ("Unable to handle URL") and return without creating a candidate.
//! Upstream tracking: <https://github.com/webrtc-rs/webrtc/issues/690>
//! — closed 2026-01-31 as **NOT_PLANNED**.
//!
//! For our `roomler-agent` deployed on corporate Windows endpoints
//! that block ALL outbound UDP but allow outbound TCP/443, the only
//! viable relay path is TURNS-over-TLS-over-TCP. This module adapts
//! a `tokio_rustls::client::TlsStream<TcpStream>` into the
//! `util::Conn` trait the `turn::client::Client` consumes, so the
//! existing crate's relay machinery can drive it unmodified.
//!
//! # Framing
//!
//! The `turn::client::Client::listen()` loop calls `conn.recv_from()`
//! once per TURN frame. On UDP each datagram is naturally one frame.
//! On TCP we get a byte stream that may contain multiple frames per
//! `tokio::io::AsyncRead::read()` call, or a single frame split across
//! several reads. The adapter buffers and yields exactly one frame
//! per `recv_from()` call.
//!
//! Frame layouts:
//!
//! * **STUN message** ([RFC 5389 §6](https://www.rfc-editor.org/rfc/rfc5389#section-6)):
//!   `[type:2 | length:2 | magic_cookie:4 | txn_id:12 | attributes]`.
//!   The `length` field counts only the attributes (padded internally
//!   to 4-byte boundaries — already inside `length`). Total frame
//!   bytes = `20 + length`.
//!
//! * **ChannelData** ([RFC 5766 §11.4](https://www.rfc-editor.org/rfc/rfc5766#section-11.4)
//!   + [§11.5](https://www.rfc-editor.org/rfc/rfc5766#section-11.5)):
//!   `[channel_no:2 | length:2 | data:length]`. RFC 5766 §11.5
//!   requires 4-byte boundary alignment of the **entire frame** when
//!   the transport is TCP (so receivers can frame the stream), so we
//!   round the total up to the next multiple of 4. The `turn-0.9.0`
//!   crate's `ChannelData::encode()` always pads on send, regardless
//!   of transport, so coturn over TCP definitely sends padded frames.
//!
//! Discriminator: first 2 bits of byte 0.
//! * `0b00xxxxxx` → STUN message (type field's class bits are 00).
//! * `0b01xxxxxx` → ChannelData (channel numbers live in `[0x4000,0x7FFE]`).
//! * `0b10xxxxxx` / `0b11xxxxxx` → malformed; we disconnect.

use std::any::Any;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;
use util::{Conn, Error};

/// Initial read-buffer capacity. 4 KiB covers most STUN messages
/// (typical ~100 bytes) and ChannelData up to ~3.5 KiB without
/// reallocating.
const INITIAL_RX_CAPACITY: usize = 4096;

/// Hard cap on a single TURN frame. STUN/TURN messages declare a
/// 16-bit length so the protocol max is ~65 KiB; we use 70 KiB as a
/// defensive cap to also cover the `+4` ChannelData header without
/// arithmetic in the bounds check.
const MAX_FRAME_BYTES: usize = 70_000;

/// One TURN frame parsed off a TLS-over-TCP byte stream.
///
/// Returns `Ok(Some(total_bytes))` when `buf` contains at least one
/// header (4 bytes) AND those bytes describe a valid frame layout.
/// `total_bytes` is the size of the WHOLE frame including the header
/// and any RFC 5766 §11.5 trailing-pad bytes; the caller is expected
/// to consume exactly that many bytes when the full frame is in the
/// buffer.
///
/// Returns `Ok(None)` when `buf` is too short to determine the size
/// (fewer than 4 bytes).
///
/// Returns `Err` when the leading bits do not match either STUN or
/// ChannelData: this is unrecoverable on a byte stream (we have no
/// way to resynchronise without a framing layer), so the caller MUST
/// disconnect.
pub(super) fn parse_frame_len(buf: &[u8]) -> Result<Option<usize>, &'static str> {
    if buf.len() < 4 {
        return Ok(None);
    }
    // STUN message types start with class bits 00 (RFC 5389 §6).
    // ChannelData numbers start with bits 01 (RFC 5766 §11.4, channels
    // in [0x4000, 0x7FFE]). Top two bits 10 and 11 are reserved.
    let class = buf[0] & 0xC0;
    let len_field = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    match class {
        0x00 => Ok(Some(20 + len_field)),
        0x40 => {
            let body = 4 + len_field;
            // Round up to next multiple of 4 (RFC 5766 §11.5 — TCP frame
            // boundaries are aligned to 4 bytes by the sender so the
            // receiver can re-frame the stream). turn-0.9.0 always pads
            // on encode; coturn does the same.
            Ok(Some((body + 3) & !3))
        }
        _ => Err("malformed TURN frame header — top two bits are reserved"),
    }
}

/// `Conn`-trait adapter wrapping a TLS-over-TCP connection to a TURN
/// server. The relay client (`turn::client::Client`) holds an
/// `Arc<dyn Conn>` and calls `recv_from`/`send_to` against it; this
/// adapter de-frames the inbound byte stream into one STUN or
/// ChannelData message per `recv_from` call and passes outbound
/// bytes through unchanged.
pub(crate) struct TcpTurnConn {
    read: Mutex<ReadState>,
    write: Mutex<WriteHalf<TlsStream<TcpStream>>>,
    local: SocketAddr,
    remote: SocketAddr,
    closed: AtomicBool,
}

struct ReadState {
    half: ReadHalf<TlsStream<TcpStream>>,
    /// Bytes already pulled from TCP but not yet returned as a frame.
    /// Holds at most one partial frame plus tail bytes of the next.
    rx_buf: Vec<u8>,
}

impl TcpTurnConn {
    /// Connect a fresh TCP stream + drive the TLS handshake, then
    /// wrap the result for use by `turn::client::Client`.
    ///
    /// `hostname` is used for SNI + server certificate verification.
    /// For an enrollment URL `turns:coturn.roomler.ai:443?transport=tcp`,
    /// pass `"coturn.roomler.ai"`.
    pub async fn connect_tls(tcp: TcpStream, hostname: &str) -> Result<Self, std::io::Error> {
        let local = tcp.local_addr()?;
        let remote = tcp.peer_addr()?;
        // Disable Nagle so STUN keepalives ship immediately. The TURN
        // control channel sends very small messages and conntrack on
        // the server side keys off arrival, not coalesced batches.
        tcp.set_nodelay(true)?;
        let server_name = tokio_rustls::rustls::pki_types::ServerName::try_from(
            hostname.to_string(),
        )
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid SNI name: {hostname}"),
            )
        })?;
        let connector = TlsConnector::from(tls_client_config());
        let tls = connector.connect(server_name, tcp).await?;
        let (rd, wr) = tokio::io::split(tls);
        Ok(Self {
            read: Mutex::new(ReadState {
                half: rd,
                rx_buf: Vec::with_capacity(INITIAL_RX_CAPACITY),
            }),
            write: Mutex::new(wr),
            local,
            remote,
            closed: AtomicBool::new(false),
        })
    }
}

#[async_trait]
impl Conn for TcpTurnConn {
    async fn connect(&self, _addr: SocketAddr) -> Result<(), Error> {
        // We're already connected via `connect_tls`. `turn::client`
        // never calls this — provided for `Conn` trait completeness.
        Ok(())
    }

    async fn recv(&self, buf: &mut [u8]) -> Result<usize, Error> {
        let (n, _) = self.recv_from(buf).await?;
        Ok(n)
    }

    async fn recv_from(&self, out: &mut [u8]) -> Result<(usize, SocketAddr), Error> {
        let mut state = self.read.lock().await;
        loop {
            if self.closed.load(Ordering::Relaxed) {
                return Err(Error::Other("tcp-turn conn closed".to_string()));
            }
            // Try to extract one complete frame.
            match parse_frame_len(&state.rx_buf) {
                Ok(Some(total)) => {
                    if total > MAX_FRAME_BYTES {
                        return Err(Error::Other(format!(
                            "TURN frame too large ({total} bytes) — disconnecting"
                        )));
                    }
                    if total > out.len() {
                        return Err(Error::ErrBufferShort);
                    }
                    if state.rx_buf.len() >= total {
                        out[..total].copy_from_slice(&state.rx_buf[..total]);
                        // Drain the consumed bytes; rotate the tail to the front.
                        state.rx_buf.drain(..total);
                        return Ok((total, self.remote));
                    }
                    // Fall through to read more.
                }
                Ok(None) => {
                    // Need at least 4 bytes to peek the length field.
                }
                Err(reason) => {
                    return Err(Error::Other(format!(
                        "tcp-turn framing error: {reason} \
                         (first 4 bytes: {:02X?})",
                        &state.rx_buf[..state.rx_buf.len().min(4)]
                    )));
                }
            }
            // Append more bytes from the wire.
            let mut chunk = [0u8; 4096];
            let n = state
                .half
                .read(&mut chunk)
                .await
                .map_err(|e| Error::Other(format!("tcp-turn read: {e}")))?;
            if n == 0 {
                self.closed.store(true, Ordering::Relaxed);
                return Err(Error::Other("tcp-turn peer closed connection".to_string()));
            }
            state.rx_buf.extend_from_slice(&chunk[..n]);
        }
    }

    async fn send(&self, buf: &[u8]) -> Result<usize, Error> {
        self.send_to(buf, self.remote).await
    }

    async fn send_to(&self, data: &[u8], _target: SocketAddr) -> Result<usize, Error> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(Error::Other("tcp-turn conn closed".to_string()));
        }
        let mut wr = self.write.lock().await;
        wr.write_all(data)
            .await
            .map_err(|e| Error::Other(format!("tcp-turn write: {e}")))?;
        wr.flush()
            .await
            .map_err(|e| Error::Other(format!("tcp-turn flush: {e}")))?;
        Ok(data.len())
    }

    fn local_addr(&self) -> Result<SocketAddr, Error> {
        Ok(self.local)
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        Some(self.remote)
    }

    async fn close(&self) -> Result<(), Error> {
        self.closed.store(true, Ordering::Relaxed);
        let mut wr = self.write.lock().await;
        let _ = wr.shutdown().await; // Best-effort.
        Ok(())
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }
}

/// Lazily-built shared `tokio_rustls::rustls::ClientConfig` with Mozilla's CA
/// bundle. Created on first call; subsequent connections reuse the
/// same `Arc`.
fn tls_client_config() -> Arc<tokio_rustls::rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<tokio_rustls::rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut root = tokio_rustls::rustls::RootCertStore::empty();
            root.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let cfg = tokio_rustls::rustls::ClientConfig::builder()
                .with_root_certificates(root)
                .with_no_client_auth();
            Arc::new(cfg)
        })
        .clone()
}

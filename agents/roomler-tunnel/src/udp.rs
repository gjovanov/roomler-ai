//! Client-side SOCKS5 UDP ASSOCIATE relay (RFC 1928 §7).
//!
//! On a UDP ASSOCIATE the client binds a local UDP relay socket, returns
//! its address in the SOCKS reply, and keeps the app's TCP control
//! connection open (the association lives as long as that TCP conn).
//! The app then sends SOCKS-UDP-framed datagrams
//! (`[RSV|FRAG|ATYP|DST|PORT|DATA]`) to the relay socket; each is parsed
//! into `(target, data)` and forwarded over the tunnel as a **UDP flow**
//! — one flow per distinct `(host, port)` the app addresses. Return
//! datagrams from the agent are re-framed with the target as the source
//! address and delivered back to the app.
//!
//! Each flow is gated server-side with `proto = udp` exactly like a TCP
//! CONNECT forward, then pumped over the negotiated transport: the
//! WebRTC DataChannel pool (one `mux`-framed message per datagram) or a
//! per-flow QUIC bidi stream (`[u16 len | datagram]`). No half-close —
//! UDP flows idle-close ([`tunnel_core::forward::UDP_FLOW_IDLE_TIMEOUT`])
//! or die with the association.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, bail};
use bson::oid::ObjectId;
use roomler_ai_remote_control::signaling::{ClientMsg, CloseReason};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{debug, info, warn};

use tunnel_core::forward::{
    FlowDemux, MAX_UDP_DATAGRAM, UDP_FLOW_IDLE_TIMEOUT, deframe_udp_datagram, quic_read_datagram,
    quic_write_datagram, send_udp_datagram_dc,
};
use tunnel_core::transport::quic::{self, QuicConnection};

use crate::forward::{FLOW_OPEN_TIMEOUT_SHARED, ForwardReply, ReplyRegistry};

/// The data plane a UDP association's flows ride, mirroring the session's
/// negotiated transport. Cloned per-flow into the spawned pump.
pub(crate) enum AssocCarrier {
    /// WebRTC-DC pool — flows multiplex onto the shared DCs by `flow_id`.
    Dc { demuxes: Arc<Vec<FlowDemux>> },
    /// QUIC — each flow is its own bidirectional stream on the session's
    /// connection.
    Quic { conn: Arc<QuicConnection> },
}

/// Outbound datagram-channel capacity per flow. A slow/stuck flow drops
/// beyond this (UDP is lossy — dropping is correct, blocking is not).
const FLOW_OUTBOX_CAP: usize = 256;

/// Per-flow outbound sender: the association's recv loop hands parsed
/// datagrams to the flow's pump via this.
type FlowTx = mpsc::Sender<Vec<u8>>;

/// Drive one SOCKS5 UDP ASSOCIATE association to completion. Binds the
/// relay socket, replies with its address, then relays datagrams until
/// the app's TCP control connection (`tcp`) closes.
pub(crate) async fn handle_associate(
    mut tcp: TcpStream,
    session_id: ObjectId,
    carrier: AssocCarrier,
    reply_registry: ReplyRegistry,
    outbound_tx: mpsc::Sender<ClientMsg>,
    flow_counter: Arc<AtomicU32>,
) -> Result<()> {
    let relay = Arc::new(
        UdpSocket::bind(("127.0.0.1", 0))
            .await
            .context("bind udp relay socket")?,
    );
    let relay_addr = relay.local_addr().context("relay local_addr")?;
    crate::socks5::reply_bound(&mut tcp, crate::socks5::REP_SUCCESS, relay_addr).await;
    info!(%session_id, %relay_addr, "socks5 UDP ASSOCIATE relay bound");

    // target (host,port) → the flow's outbound datagram sender.
    let flows: Arc<Mutex<HashMap<(String, u16), FlowTx>>> = Arc::new(Mutex::new(HashMap::new()));
    // The app's source addr, latched on the first datagram — return
    // datagrams go here; datagrams from other sources are dropped.
    let mut app_src: Option<SocketAddr> = None;
    // +512 header slack over the max datagram for the SOCKS UDP prefix.
    let mut buf = vec![0u8; MAX_UDP_DATAGRAM + 512];

    loop {
        tokio::select! {
            // The app's TCP control connection closing ends the whole
            // association (RFC 1928). Reading it also drains anything the
            // app writes (it shouldn't write payload — only close).
            r = drain_control(&mut tcp) => {
                debug!(%session_id, ?r, "socks5 UDP control connection closed; ending association");
                break;
            }
            recvd = relay.recv_from(&mut buf) => {
                let (n, from) = match recvd {
                    Ok(x) => x,
                    Err(e) => { warn!(%session_id, %e, "udp relay recv_from failed"); continue; }
                };
                let src = *app_src.get_or_insert(from);
                if from != src {
                    debug!(%session_id, %from, %src, "udp datagram from unexpected source — dropping");
                    continue;
                }
                let (host, port, off) = match crate::socks5::parse_udp_datagram(&buf[..n]) {
                    Ok(x) => x,
                    Err(e) => { debug!(%session_id, %e, "malformed socks udp datagram — dropping"); continue; }
                };
                let data = buf[off..n].to_vec();
                let tx = get_or_open_flow(
                    &flows,
                    &carrier,
                    session_id,
                    &host,
                    port,
                    src,
                    &relay,
                    &reply_registry,
                    &outbound_tx,
                    &flow_counter,
                )
                .await;
                // Lossy hand-off — a full/closed outbox drops the datagram.
                if let Err(e) = tx.try_send(data) {
                    debug!(%session_id, %host, port, %e, "udp flow outbox full/closed — dropping datagram");
                }
            }
        }
    }
    // Dropping `flows` drops every flow's outbound sender → each pump
    // sees its channel close and tears down (closing its QUIC stream /
    // unregistering its demux mailbox).
    Ok(())
}

/// Look up the flow for `(host, port)`; open + spawn its pump if absent.
/// Returns the flow's outbound sender.
#[allow(clippy::too_many_arguments)]
async fn get_or_open_flow(
    flows: &Arc<Mutex<HashMap<(String, u16), FlowTx>>>,
    carrier: &AssocCarrier,
    session_id: ObjectId,
    host: &str,
    port: u16,
    app_src: SocketAddr,
    relay: &Arc<UdpSocket>,
    reply_registry: &ReplyRegistry,
    outbound_tx: &mpsc::Sender<ClientMsg>,
    flow_counter: &Arc<AtomicU32>,
) -> FlowTx {
    let key = (host.to_string(), port);
    let mut map = flows.lock().await;
    if let Some(tx) = map.get(&key) {
        return tx.clone();
    }
    let flow_id = flow_counter.fetch_add(1, Ordering::Relaxed);
    let (ftx, frx) = mpsc::channel::<Vec<u8>>(FLOW_OUTBOX_CAP);
    map.insert(key.clone(), ftx.clone());

    let flows2 = Arc::clone(flows);
    let relay2 = Arc::clone(relay);
    let registry2 = Arc::clone(reply_registry);
    let outbound2 = outbound_tx.clone();
    let host_owned = host.to_string();
    match carrier {
        AssocCarrier::Dc { demuxes } => {
            let demuxes = Arc::clone(demuxes);
            tokio::spawn(async move {
                run_flow_dc(
                    session_id, flow_id, host_owned, port, demuxes, relay2, app_src, frx,
                    registry2, outbound2,
                )
                .await;
                flows2.lock().await.remove(&key);
            });
        }
        AssocCarrier::Quic { conn } => {
            let conn = Arc::clone(conn);
            tokio::spawn(async move {
                run_flow_quic(
                    session_id, flow_id, host_owned, port, conn, relay2, app_src, frx, registry2,
                    outbound2,
                )
                .await;
                flows2.lock().await.remove(&key);
            });
        }
    }
    ftx
}

/// Send a `UdpForwardRequest` and await the server's accept/reject
/// (routed into `reply_registry` by the session dispatch loop). Returns
/// the accepted `dc_index` (meaningful for the DC transport; 0 for QUIC).
async fn open_udp_flow(
    session_id: ObjectId,
    flow_id: u32,
    host: &str,
    port: u16,
    reply_registry: &ReplyRegistry,
    outbound_tx: &mpsc::Sender<ClientMsg>,
) -> Result<u8> {
    let (reply_tx, reply_rx) = oneshot::channel::<ForwardReply>();
    reply_registry.lock().await.insert(flow_id, reply_tx);
    outbound_tx
        .send(ClientMsg::UdpForwardRequest {
            session_id,
            flow_id,
            dst_host: host.to_string(),
            dst_port: port,
        })
        .await
        .context("send UdpForwardRequest")?;
    let reply = match tokio::time::timeout(FLOW_OPEN_TIMEOUT_SHARED, reply_rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => {
            reply_registry.lock().await.remove(&flow_id);
            bail!("udp reply oneshot dropped — dispatcher exited?");
        }
        Err(_) => {
            reply_registry.lock().await.remove(&flow_id);
            bail!("UdpForwardRequest timed out after {FLOW_OPEN_TIMEOUT_SHARED:?}");
        }
    };
    match reply {
        ForwardReply::Accept { dc_index } => Ok(dc_index),
        ForwardReply::Reject { kind, reason } => bail!("udp forward rejected: {kind:?} {reason}"),
    }
}

/// One UDP flow over the WebRTC-DC pool. Datagrams from the app
/// (`outbound_rx`) are framed + sent on the DC; datagrams off the DC are
/// re-framed with the target as source and delivered to the app.
#[allow(clippy::too_many_arguments)]
async fn run_flow_dc(
    session_id: ObjectId,
    flow_id: u32,
    host: String,
    port: u16,
    demuxes: Arc<Vec<FlowDemux>>,
    relay: Arc<UdpSocket>,
    app_src: SocketAddr,
    mut outbound_rx: mpsc::Receiver<Vec<u8>>,
    reply_registry: ReplyRegistry,
    outbound_tx: mpsc::Sender<ClientMsg>,
) {
    let dc_index = match open_udp_flow(
        session_id,
        flow_id,
        &host,
        port,
        &reply_registry,
        &outbound_tx,
    )
    .await
    {
        Ok(i) => i,
        Err(e) => {
            warn!(%session_id, flow_id, %host, port, %e, "udp flow open failed");
            return;
        }
    };
    let Some(demux) = demuxes.get(dc_index as usize).cloned() else {
        warn!(%session_id, flow_id, dc_index, "server returned out-of-range dc_index for udp flow");
        return;
    };
    let (mut from_dc, _stats) = demux.register(flow_id).await;
    let dc = demux.dc();
    debug!(%session_id, flow_id, dc_index, %host, port, "udp flow open (dc)");

    let reason = loop {
        tokio::select! {
            out = outbound_rx.recv() => match out {
                Some(dg) => {
                    if let Err(e) = send_udp_datagram_dc(&dc, flow_id, &dg).await {
                        debug!(%session_id, flow_id, %e, "udp flow DC send failed");
                        break CloseReason::IoError;
                    }
                }
                None => break CloseReason::ClientShutdown,
            },
            inb = from_dc.recv() => match inb {
                Some(bytes) => {
                    if let Some(dg) = deframe_udp_datagram(&bytes) {
                        let framed = crate::socks5::encode_udp_datagram(&host, port, dg);
                        if let Err(e) = relay.send_to(&framed, app_src).await {
                            debug!(%session_id, flow_id, %e, "udp flow relay send_to app failed");
                        }
                    }
                }
                None => break CloseReason::Eof,
            },
            _ = tokio::time::sleep(UDP_FLOW_IDLE_TIMEOUT) => break CloseReason::IdleTimeout,
        }
    };

    demux.unregister(flow_id).await;
    debug!(%session_id, flow_id, ?reason, "udp flow ended (dc)");
    let _ = outbound_tx
        .send(ClientMsg::UdpClosed {
            session_id,
            flow_id,
            reason,
        })
        .await;
}

/// One UDP flow over a native QUIC bidi stream.
#[allow(clippy::too_many_arguments)]
async fn run_flow_quic(
    session_id: ObjectId,
    flow_id: u32,
    host: String,
    port: u16,
    conn: Arc<QuicConnection>,
    relay: Arc<UdpSocket>,
    app_src: SocketAddr,
    mut outbound_rx: mpsc::Receiver<Vec<u8>>,
    reply_registry: ReplyRegistry,
    outbound_tx: mpsc::Sender<ClientMsg>,
) {
    if let Err(e) = open_udp_flow(
        session_id,
        flow_id,
        &host,
        port,
        &reply_registry,
        &outbound_tx,
    )
    .await
    {
        warn!(%session_id, flow_id, %host, port, %e, "udp flow open failed");
        return;
    }
    let (mut send, mut recv) = match quic::open_flow(&conn, flow_id).await {
        Ok(s) => s,
        Err(e) => {
            warn!(%session_id, flow_id, %e, "udp flow quic open_flow failed");
            return;
        }
    };
    debug!(%session_id, flow_id, %host, port, "udp flow open (quic)");

    let reason = loop {
        tokio::select! {
            out = outbound_rx.recv() => match out {
                Some(dg) => {
                    if let Err(e) = quic_write_datagram(&mut send, &dg).await {
                        debug!(%session_id, flow_id, %e, "udp flow quic write failed");
                        break CloseReason::IoError;
                    }
                }
                None => break CloseReason::ClientShutdown,
            },
            inb = quic_read_datagram(&mut recv) => match inb {
                Ok(Some(dg)) => {
                    let framed = crate::socks5::encode_udp_datagram(&host, port, &dg);
                    if let Err(e) = relay.send_to(&framed, app_src).await {
                        debug!(%session_id, flow_id, %e, "udp flow relay send_to app failed");
                    }
                }
                Ok(None) => break CloseReason::Eof,
                Err(e) => {
                    debug!(%session_id, flow_id, %e, "udp flow quic read failed");
                    break CloseReason::IoError;
                }
            },
            _ = tokio::time::sleep(UDP_FLOW_IDLE_TIMEOUT) => break CloseReason::IdleTimeout,
        }
    };

    debug!(%session_id, flow_id, ?reason, "udp flow ended (quic)");
    let _ = outbound_tx
        .send(ClientMsg::UdpClosed {
            session_id,
            flow_id,
            reason,
        })
        .await;
}

/// Read + discard the SOCKS control connection until EOF/error. RFC 1928
/// says nothing meaningful flows on it after the ASSOCIATE reply; its
/// close is the association's teardown signal.
async fn drain_control(tcp: &mut TcpStream) -> std::io::Result<()> {
    let mut b = [0u8; 256];
    loop {
        match tcp.read(&mut b).await {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
}

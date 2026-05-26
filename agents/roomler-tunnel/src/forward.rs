//! `roomler-tunnel forward` — open one TCP forward through an enrolled
//! agent. The TeamViewer-shaped flow:
//!
//! 1. WS-connect to `wss://<server>/ws?role=tunnel-client&token=<jwt>`.
//! 2. `rc:tunnel.hello { role: TunnelClient, version, supported_transports }`.
//! 3. `rc:tunnel.open { agent_id, transport: "webrtc-dc-v1" }`.
//!    Wait for `rc:tunnel.opened` → carries `session_id` + `ice_servers`.
//! 4. Build a [`TunnelPeer`] with those ICE servers. Generate SDP
//!    offer; ship over `rc:sdp.offer { session_id, sdp }`.
//!    Trickle ICE via `rc:ice { session_id, candidate }`.
//! 5. Receive `rc:sdp.answer { session_id, sdp }`; remote-describe.
//!    Wait for the DC pool to fully open.
//! 6. Install [`FlowDemux`] on each DC.
//! 7. Bind a local TCP listener on `--local`. Per accepted conn:
//!    (a) assign `flow_id` (monotonic) + `dc_index` (round-robin);
//!    (b) send `rc:tunnel.tcp.request` and park a oneshot on `flow_id`;
//!    (c) on Accept: register the flow with the chosen DC's demux and
//!    spawn `tunnel_core::forward::run_flow` with a half-close
//!    callback that pushes `rc:tunnel.tcp.half_close` over WS;
//!    (d) on Reject: log + close the local TCP.
//!
//! Server-side relay (T2.10c) is NOT yet implemented — the server's
//! `ws/tunnel.rs::handle_tcp_forward_request` synthesises an Accept
//! with `dc_index: 0` for now. End-to-end smoke against a real agent
//! waits for that wiring; until then this CLI exercises everything
//! up to and including the SDP/ICE handshake + per-flow Accept reply.

use anyhow::{Context, Result, bail};
use bson::oid::ObjectId;
use futures::{SinkExt, StreamExt};
use roomler_ai_remote_control::signaling::{
    ClientMsg, CloseReason, Direction, RejectKind, ServerMsg, TunnelRole,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};
use tunnel_core::forward::{FlowDemux, HalfCloseSink, run_flow};
use tunnel_core::transport::webrtc_dc::TunnelPeer;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;

use crate::config::{TunnelConfig, derive_ws_url};

/// Buffer depth for the outbound WS channel. Generous — most flow
/// activity is ICE candidate trickle + per-flow Accept replies, both
/// modest. Sized to absorb a burst at session-open without blocking.
const WS_OUT_CHANNEL_DEPTH: usize = 256;

/// Cap on how long we wait for `rc:tunnel.opened` after sending
/// `rc:tunnel.open`. Round-trip + server-side cross-tenant gate.
const TUNNEL_OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Cap on the SDP / ICE / DC-pool handshake. Includes ICE gathering
/// plus relay candidate establishment which can take a few seconds
/// on TURN paths.
const PEER_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Cap on per-flow `TcpForwardRequest` → `Accept/Reject` round-trip.
/// Server-side ACL eval is local but the request rides the agent's
/// dial timeout in the relay case.
const FLOW_OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Reply registry: per-flow oneshot for the server's accept/reject.
type ReplyRegistry = Arc<Mutex<HashMap<u32, oneshot::Sender<ForwardReply>>>>;

/// Active-flow registry: which DC index a given flow is bound to, so
/// the WS dispatch can route inbound `TcpHalfClose` audit signals
/// (no demux action — in-band marker handles the data-plane close).
type ActiveFlows = Arc<Mutex<HashMap<u32, u8>>>;

#[derive(Debug)]
enum ForwardReply {
    Accept { dc_index: u8 },
    Reject { kind: RejectKind, reason: String },
}

/// Entry point for `roomler-tunnel forward`.
pub async fn run(cfg: TunnelConfig, agent_hex: &str, local: u16, remote: &str) -> Result<()> {
    let agent_id = ObjectId::parse_str(agent_hex)
        .with_context(|| format!("--agent must be a 24-hex ObjectId, got {agent_hex}"))?;
    let (dst_host, dst_port) = parse_remote(remote)?;

    info!(
        server = %cfg.server_url,
        agent = %agent_id,
        local,
        dst_host,
        dst_port,
        "roomler-tunnel forward starting"
    );

    // ────────────── WS connect ─────────────────────────────────────
    let ws_base = derive_ws_url(&cfg.server_url)?;
    let ws_url = format!(
        "{ws_base}?role=tunnel-client&token={}",
        urlencoding_lite(&cfg.tunnel_client_token)
    );
    info!(%ws_base, "connecting websocket");
    let (ws_stream, _resp) = connect_async(&ws_url)
        .await
        .with_context(|| format!("WS connect to {ws_base}"))?;
    info!("websocket connected");
    let (mut ws_sink, mut ws_source) = ws_stream.split();

    // Outbound channel — any task pushes ClientMsg here; a single
    // task drains it onto the WS sink.
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<ClientMsg>(WS_OUT_CHANNEL_DEPTH);

    let _sender_task = tokio::spawn(async move {
        while let Some(msg) = outbound_rx.recv().await {
            let json = match serde_json::to_string(&msg) {
                Ok(s) => s,
                Err(e) => {
                    warn!(%e, "outbound serialise failed");
                    continue;
                }
            };
            if let Err(e) = ws_sink.send(Message::text(json)).await {
                warn!(%e, "outbound WS send failed; dropping");
                break;
            }
        }
        debug!("outbound WS task exiting");
    });

    // Say hello.
    outbound_tx
        .send(ClientMsg::TunnelHello {
            role: TunnelRole::Client,
            version: env!("CARGO_PKG_VERSION").to_string(),
            supported_transports: vec!["webrtc-dc-v1".to_string()],
        })
        .await
        .context("send TunnelHello")?;

    // Open the tunnel.
    outbound_tx
        .send(ClientMsg::TunnelOpen {
            agent_id,
            transport: "webrtc-dc-v1".to_string(),
        })
        .await
        .context("send TunnelOpen")?;

    // ────────────── Wait for `rc:tunnel.opened` ────────────────────
    let opened = tokio::time::timeout(TUNNEL_OPEN_TIMEOUT, async {
        loop {
            let parsed = recv_server_msg(&mut ws_source).await?;
            match parsed {
                ServerMsg::TunnelOpened {
                    session_id,
                    transport,
                    dc_pool_size,
                    sctp_rwnd_bytes,
                    ice_servers,
                } => {
                    info!(
                        %session_id, %transport, dc_pool_size, sctp_rwnd_bytes,
                        ice_servers = ice_servers.len(),
                        "rc:tunnel.opened"
                    );
                    break anyhow::Ok((session_id, ice_servers));
                }
                ServerMsg::TunnelRevoked { reason } => {
                    bail!("tunnel revoked by server during open: {reason}");
                }
                ServerMsg::Error {
                    session_id: _,
                    code,
                    message,
                } => {
                    bail!("server error during tunnel.open: {code}: {message}");
                }
                other => debug!(?other, "ignoring pre-opened ServerMsg"),
            }
        }
    })
    .await
    .context("waiting for rc:tunnel.opened")??;
    let (session_id, ice_servers) = opened;

    // ────────────── Build TunnelPeer + SDP/ICE handshake ───────────
    let rtc_ice_servers: Vec<RTCIceServer> = ice_servers
        .into_iter()
        .map(|ice| RTCIceServer {
            urls: ice.urls,
            username: ice.username.unwrap_or_default(),
            credential: ice.credential.unwrap_or_default(),
        })
        .collect();

    let peer = TunnelPeer::new(rtc_ice_servers)
        .await
        .context("constructing TunnelPeer")?;

    // Trickle ICE upstream.
    {
        let outbound = outbound_tx.clone();
        peer.on_local_ice_candidate(move |c| {
            let outbound = outbound.clone();
            Box::pin(async move {
                let Some(c) = c else {
                    debug!("ICE gathering complete (local)");
                    return;
                };
                let init = match c.to_json() {
                    Ok(i) => i,
                    Err(e) => {
                        warn!(%e, "local candidate to_json failed");
                        return;
                    }
                };
                let candidate = match serde_json::to_value(&init) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(%e, "local candidate serialise failed");
                        return;
                    }
                };
                if let Err(e) = outbound
                    .send(ClientMsg::TunnelIce {
                        session_id,
                        candidate,
                    })
                    .await
                {
                    warn!(%e, "ICE trickle send failed");
                }
            })
        });
    }

    let offer = peer.create_offer().await.context("create_offer")?;
    outbound_tx
        .send(ClientMsg::TunnelSdpOffer {
            session_id,
            sdp: offer.sdp.clone(),
        })
        .await
        .context("send TunnelSdpOffer")?;

    // Spawn the WS dispatcher. It handles every inbound ServerMsg
    // from this point on (SdpAnswer, Ice, TcpForwardAccept/Reject,
    // TcpHalfClose audit, TcpClosed audit, TunnelTerminate,
    // TunnelRevoked).
    let reply_registry: ReplyRegistry = Arc::new(Mutex::new(HashMap::new()));
    let active_flows: ActiveFlows = Arc::new(Mutex::new(HashMap::new()));
    let peer_for_dispatch = Arc::new(peer);
    let pool_ready = Arc::new(tokio::sync::Notify::new());

    let _dispatcher_task = {
        let peer = Arc::clone(&peer_for_dispatch);
        let reply_registry = Arc::clone(&reply_registry);
        let active_flows = Arc::clone(&active_flows);
        let pool_ready = Arc::clone(&pool_ready);
        let outbound_tx = outbound_tx.clone();
        tokio::spawn(async move {
            dispatch_loop(
                &mut ws_source,
                &peer,
                session_id,
                reply_registry,
                active_flows,
                pool_ready,
                outbound_tx,
            )
            .await
        })
    };

    // ────────────── Wait for pool open ─────────────────────────────
    tokio::time::timeout(PEER_READY_TIMEOUT, peer_for_dispatch.wait_pool_open())
        .await
        .context("waiting for DC pool to open")?
        .context("DC pool open failed")?;
    info!(
        "DC pool fully open ({} channels)",
        peer_for_dispatch.pool_size()
    );
    pool_ready.notify_waiters();

    // Install one FlowDemux per DC. Hold them in a Vec so the local
    // TCP listener can borrow by dc_index.
    let mut demuxes: Vec<FlowDemux> = Vec::with_capacity(peer_for_dispatch.pool_size() as usize);
    for idx in 0..peer_for_dispatch.pool_size() {
        let dc = peer_for_dispatch
            .dc(idx)
            .with_context(|| format!("dc({idx}) returned None after pool_open"))?;
        demuxes.push(FlowDemux::install(dc).await);
    }
    let demuxes = Arc::new(demuxes);

    // ────────────── Local TCP listener ─────────────────────────────
    let listener = TcpListener::bind(("127.0.0.1", local))
        .await
        .with_context(|| format!("binding 127.0.0.1:{local}"))?;
    info!(local = %listener.local_addr()?, "listening for local TCP connections");

    let flow_counter = Arc::new(AtomicU32::new(1));
    let rr_counter = Arc::new(AtomicUsize::new(0));

    loop {
        let (tcp, peer_addr) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                error!(%e, "accept failed");
                continue;
            }
        };
        // P0 throughput fix (rc.64, field-repro 2026-05-26): disable
        // Nagle on the local listener's accepted TCP socket. The agent
        // side already sets TCP_NODELAY on its outbound (corp-side)
        // dialer (see agents/roomler-agent/src/tunnel/dialer.rs); the
        // asymmetry meant TDS row tokens flowing FROM the server,
        // through the DC, OUT to the local SSMS/psql/JDBC client got
        // Nagle-coalesced on this socket. Under MSSQL TDS the small
        // row tokens batch up waiting for ACKs that don't come until
        // ~40 ms later (delayed ACK + Nagle interaction), collapsing
        // sustained throughput to tens of KB/s and triggering server-
        // side ASYNC_NETWORK_IO suspensions. Setting nodelay is
        // canonical for tunnels; no downside.
        if let Err(e) = tcp.set_nodelay(true) {
            warn!(%peer_addr, %e, "set_nodelay(true) on local TCP failed");
        }
        // rc.66 throughput follow-on: bump SO_SNDBUF on the accepted
        // loopback socket from the OS default (Windows: 64 KiB-ish,
        // can be as low as 8 KiB on some kernels) to 4 MiB. Windows
        // loopback under TDS bulk-read fills the default send buffer
        // in milliseconds; once full, every `write_all` in
        // `pump_dc_to_tcp` blocks waiting for the local app to read,
        // and that backpressures all the way up the chain. A 4 MiB
        // ceiling absorbs the burst so the producer can keep pumping
        // while the consumer drains. Best-effort: Windows may cap
        // below 4 MiB silently (autotune); the actual ceiling is
        // observable via `getsockopt` if needed, but the request
        // alone is enough to lift the floor.
        //
        // Uses `socket2` indirectly via `tokio::net::TcpStream::set_send_buffer_size`
        // when available; pre-1.41 tokio paths fall back through
        // `as_raw_socket` / `WSAIoctl` on Windows. We're on tokio 1.x
        // recent enough that `set_send_buffer_size` is exposed.
        const LOCAL_SNDBUF_BYTES: u32 = 4 * 1024 * 1024;
        // tokio 1.41+ has TcpStream::set_send_buffer_size returning
        // io::Result; older versions don't. Use a feature-detected
        // import path: socket2 on the raw fd/socket is portable.
        #[cfg(any(unix, windows))]
        {
            use socket2::SockRef;
            let sock = SockRef::from(&tcp);
            if let Err(e) = sock.set_send_buffer_size(LOCAL_SNDBUF_BYTES as usize) {
                warn!(%peer_addr, %e, "set_send_buffer_size(4MiB) on local TCP failed");
            }
        }
        debug!(%peer_addr, "accepted local TCP connection");

        let flow_id = flow_counter.fetch_add(1, Ordering::Relaxed);
        let dc_index_chosen = (rr_counter.fetch_add(1, Ordering::Relaxed) % demuxes.len()) as u8;

        // Install reply oneshot before sending the request.
        let (reply_tx, reply_rx) = oneshot::channel::<ForwardReply>();
        reply_registry.lock().await.insert(flow_id, reply_tx);

        let demuxes = Arc::clone(&demuxes);
        let reply_registry = Arc::clone(&reply_registry);
        let active_flows = Arc::clone(&active_flows);
        let outbound_tx = outbound_tx.clone();
        let dst_host = dst_host.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_local_connection(
                tcp,
                peer_addr,
                flow_id,
                dc_index_chosen,
                session_id,
                &dst_host,
                dst_port,
                outbound_tx,
                reply_rx,
                reply_registry,
                active_flows,
                demuxes,
            )
            .await
            {
                warn!(flow_id, %e, "flow ended with error");
            }
        });
    }

    // Listen loop above never returns under normal operation — Ctrl-C
    // signals tear down the runtime. The spawned `_sender_task` and
    // `_dispatcher_task` ride that teardown.
}

/// Send `TcpForwardRequest`, await accept/reject, and on accept drive
/// [`run_flow`] until it returns.
///
/// The `_dc_index_hint` is the round-robin pick from the listen loop;
/// the server is authoritative and may return a different DC index
/// in its Accept message (e.g. fairness/load-balancing across the
/// pool). The hint is currently unused but plumbed so a future
/// `rc:tunnel.tcp.request` variant can carry a preference.
#[allow(clippy::too_many_arguments)]
async fn handle_local_connection(
    tcp: tokio::net::TcpStream,
    peer_addr: std::net::SocketAddr,
    flow_id: u32,
    _dc_index_hint: u8,
    session_id: ObjectId,
    dst_host: &str,
    dst_port: u16,
    outbound_tx: mpsc::Sender<ClientMsg>,
    reply_rx: oneshot::Receiver<ForwardReply>,
    reply_registry: ReplyRegistry,
    active_flows: ActiveFlows,
    demuxes: Arc<Vec<FlowDemux>>,
) -> Result<()> {
    // Send the request.
    outbound_tx
        .send(ClientMsg::TcpForwardRequest {
            session_id,
            flow_id,
            dst_host: dst_host.to_string(),
            dst_port,
        })
        .await
        .context("send TcpForwardRequest")?;

    // Wait for reply.
    let reply = match tokio::time::timeout(FLOW_OPEN_TIMEOUT, reply_rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_canceled)) => {
            reply_registry.lock().await.remove(&flow_id);
            bail!("reply oneshot dropped — dispatcher exited?");
        }
        Err(_) => {
            reply_registry.lock().await.remove(&flow_id);
            warn!(flow_id, "TcpForwardRequest timed out");
            bail!("forward request timed out after {FLOW_OPEN_TIMEOUT:?}");
        }
    };

    let dc_index = match reply {
        ForwardReply::Accept { dc_index } => {
            info!(flow_id, dc_index, "rc:tunnel.tcp.accept");
            dc_index
        }
        ForwardReply::Reject { kind, reason } => {
            warn!(flow_id, ?kind, %reason, "rc:tunnel.tcp.reject — dropping local conn");
            drop(tcp);
            return Ok(());
        }
    };

    // Choose the demux for the dc_index the server picked. (Round-
    // robin gave us a CHOICE; server is authoritative.)
    let Some(demux) = demuxes.get(dc_index as usize) else {
        warn!(flow_id, dc_index, "server returned out-of-range dc_index");
        drop(tcp);
        bail!("server returned out-of-range dc_index {dc_index}");
    };
    let demux = demux.clone();

    let from_dc = demux.register(flow_id).await;
    active_flows.lock().await.insert(flow_id, dc_index);

    // Half-close audit callback. The in-band sentinel in the pump
    // closes the peer's mailbox; this wire message is for audit only.
    let outbound_for_audit = outbound_tx.clone();
    let on_local_eof: HalfCloseSink = Arc::new(move |fid: u32| {
        let outbound = outbound_for_audit.clone();
        // Spawn so we don't await inside a sync Fn closure.
        tokio::spawn(async move {
            let _ = outbound
                .send(ClientMsg::TcpHalfClose {
                    session_id,
                    flow_id: fid,
                    direction: Direction::SrcToDst,
                })
                .await;
        });
    });

    let dc = demux.dc();
    debug!(flow_id, dc_index, %peer_addr, "running flow");
    let close_reason = run_flow(tcp, dc, flow_id, from_dc, on_local_eof).await;
    info!(flow_id, ?close_reason, "flow ended");

    // Audit close.
    let _ = outbound_tx
        .send(ClientMsg::TcpClosed {
            session_id,
            flow_id,
            reason: close_reason,
        })
        .await;

    active_flows.lock().await.remove(&flow_id);
    demux.unregister(flow_id).await;
    Ok(())
}

/// WS read loop. Owns every inbound `ServerMsg` after the
/// `TunnelOpened` was consumed by `run()`. Forwards SDP/ICE into
/// the [`TunnelPeer`], routes per-flow accept/reject into the
/// `reply_registry`, and logs the audit-side TcpHalfClose / TcpClosed.
#[allow(clippy::too_many_arguments)]
async fn dispatch_loop(
    ws_source: &mut (
             impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin
         ),
    peer: &Arc<TunnelPeer>,
    session_id: ObjectId,
    reply_registry: ReplyRegistry,
    active_flows: ActiveFlows,
    _pool_ready: Arc<tokio::sync::Notify>,
    outbound_tx: mpsc::Sender<ClientMsg>,
) {
    while let Some(item) = ws_source.next().await {
        let text = match item {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(c)) => {
                info!(?c, "server closed WS");
                return;
            }
            Ok(Message::Ping(d)) => {
                // tokio-tungstenite handles automatic Pong replies
                // on its own, but log for diagnostics.
                debug!(len = d.len(), "ws ping");
                continue;
            }
            Ok(_) => continue,
            Err(e) => {
                warn!(%e, "ws read error; exiting dispatch loop");
                return;
            }
        };
        let parsed: ServerMsg = match serde_json::from_str(text.as_str()) {
            Ok(p) => p,
            Err(e) => {
                debug!(%e, text = %text.as_str(), "ignoring non-rc:* / unparseable frame");
                continue;
            }
        };
        match parsed {
            ServerMsg::TunnelSdpAnswer {
                session_id: sid,
                sdp,
            } if sid == session_id => {
                if let Err(e) = peer.accept_answer(&sdp).await {
                    error!(%e, "accept_answer failed");
                }
            }
            ServerMsg::TunnelIce {
                session_id: sid,
                candidate,
            } if sid == session_id => {
                let init: RTCIceCandidateInit = match serde_json::from_value(candidate) {
                    Ok(i) => i,
                    Err(e) => {
                        warn!(%e, "remote ICE candidate parse failed");
                        continue;
                    }
                };
                if let Err(e) = peer.add_remote_ice_candidate(init).await {
                    warn!(%e, "add_remote_ice_candidate failed");
                }
            }
            ServerMsg::TcpForwardAccept {
                session_id: sid,
                flow_id,
                dc_index,
            } if sid == session_id => {
                if let Some(tx) = reply_registry.lock().await.remove(&flow_id) {
                    let _ = tx.send(ForwardReply::Accept { dc_index });
                } else {
                    warn!(flow_id, "accept for unknown flow_id");
                }
            }
            ServerMsg::TcpForwardReject {
                session_id: sid,
                flow_id,
                kind,
                reason,
            } if sid == session_id => {
                if let Some(tx) = reply_registry.lock().await.remove(&flow_id) {
                    let _ = tx.send(ForwardReply::Reject { kind, reason });
                } else {
                    warn!(flow_id, ?kind, %reason, "reject for unknown flow_id");
                }
            }
            ServerMsg::TcpHalfClose {
                session_id: sid,
                flow_id,
                direction,
            } if sid == session_id => {
                // Audit only — the in-band marker on the DC drives
                // the actual data-plane close. See
                // `tunnel_core::forward` module docs.
                debug!(flow_id, ?direction, "rc:tunnel.tcp.half_close (audit)");
            }
            ServerMsg::TcpClosed {
                session_id: sid,
                flow_id,
                reason,
            } if sid == session_id => {
                debug!(flow_id, ?reason, "rc:tunnel.tcp.closed (audit)");
                active_flows.lock().await.remove(&flow_id);
            }
            ServerMsg::TunnelTerminate {
                session_id: sid,
                reason,
            } if sid == session_id => {
                info!(?reason, "rc:tunnel.terminate — peer torn down by server");
                return;
            }
            ServerMsg::TunnelRevoked { reason } => {
                error!(%reason, "rc:tunnel.revoked — admin revoked our enrollment");
                let _ = outbound_tx
                    .send(ClientMsg::TunnelTerminate {
                        session_id,
                        reason: CloseReason::ServerTerminated,
                    })
                    .await;
                return;
            }
            other => debug!(?other, "dispatch: ignoring ServerMsg"),
        }
    }
    debug!("WS source ended; dispatch loop exiting");
}

async fn recv_server_msg<S>(source: &mut S) -> Result<ServerMsg>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let Some(item) = source.next().await else {
            bail!("WS closed before any ServerMsg arrived");
        };
        match item {
            Ok(Message::Text(t)) => match serde_json::from_str::<ServerMsg>(t.as_str()) {
                Ok(m) => return Ok(m),
                Err(e) => debug!(%e, text = %t.as_str(), "ignoring unparseable WS frame"),
            },
            Ok(Message::Close(c)) => bail!("WS closed by peer: {c:?}"),
            Ok(_) => continue,
            Err(e) => return Err(e).context("ws read"),
        }
    }
}

/// Parse a `host:port` string. Robust to bracketed IPv6 (`[::1]:80`).
pub(crate) fn parse_remote(s: &str) -> Result<(String, u16)> {
    if let Some(rest) = s.strip_prefix('[') {
        // IPv6: `[addr]:port`
        let close = rest
            .find(']')
            .with_context(|| format!("--remote with `[` must close with `]:port`: {s}"))?;
        let host = &rest[..close];
        let port_str = rest[close + 1..]
            .strip_prefix(':')
            .with_context(|| format!("missing `:port` after `]`: {s}"))?;
        let port = port_str
            .parse()
            .with_context(|| format!("invalid port {port_str}"))?;
        return Ok((host.to_string(), port));
    }
    let (host, port_str) = s
        .rsplit_once(':')
        .with_context(|| format!("--remote must be host:port, got {s}"))?;
    if host.is_empty() {
        bail!("--remote host must not be empty");
    }
    let port = port_str
        .parse()
        .with_context(|| format!("invalid port {port_str}"))?;
    Ok((host.to_string(), port))
}

/// Tiny URL-encoder for the JWT in the query string. We only need
/// to escape characters that appear in JWTs (`.`, `-`, `_` are safe
/// per JWT spec; we just guard against future drift). Avoids pulling
/// the `url` crate just for this.
fn urlencoding_lite(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_remote_simple() {
        let (h, p) = parse_remote("db.intranet:5432").unwrap();
        assert_eq!(h, "db.intranet");
        assert_eq!(p, 5432);
    }

    #[test]
    fn parse_remote_ipv4() {
        let (h, p) = parse_remote("10.0.0.5:1521").unwrap();
        assert_eq!(h, "10.0.0.5");
        assert_eq!(p, 1521);
    }

    #[test]
    fn parse_remote_ipv6_bracketed() {
        let (h, p) = parse_remote("[::1]:5432").unwrap();
        assert_eq!(h, "::1");
        assert_eq!(p, 5432);
    }

    #[test]
    fn parse_remote_ipv6_with_zone_id() {
        let (h, p) = parse_remote("[fe80::1%eth0]:22").unwrap();
        assert_eq!(h, "fe80::1%eth0");
        assert_eq!(p, 22);
    }

    #[test]
    fn parse_remote_rejects_missing_port() {
        let err = parse_remote("db.intranet").unwrap_err();
        assert!(err.to_string().contains("host:port"));
    }

    #[test]
    fn parse_remote_rejects_empty_host() {
        let err = parse_remote(":5432").unwrap_err();
        assert!(err.to_string().contains("host must not be empty"));
    }

    #[test]
    fn parse_remote_rejects_invalid_port() {
        assert!(parse_remote("db.intranet:notaport").is_err());
        assert!(parse_remote("db.intranet:99999").is_err()); // u16 overflow
    }

    #[test]
    fn urlencoding_lite_preserves_jwt_chars() {
        // JWT chars are A-Z a-z 0-9 . - _ — none should be encoded.
        let jwt = "eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.payload.sig-with_under";
        assert_eq!(urlencoding_lite(jwt), jwt);
    }

    #[test]
    fn urlencoding_lite_encodes_space() {
        assert_eq!(urlencoding_lite("a b"), "a%20b");
    }
}

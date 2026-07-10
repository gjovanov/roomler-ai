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
    ClientMsg, CloseReason, Direction, IceServer, RejectKind, ServerMsg, TunnelRole,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};
use tunnel_core::forward::{FlowDemux, HalfCloseSink, run_flow, run_flow_quic};
use tunnel_core::transport::quic::{self, QuicConnection, QuicPeer};
use tunnel_core::transport::relay;
use tunnel_core::transport::webrtc_dc::TunnelPeer;
use tunnel_core::transport::{TRANSPORT_QUIC_V1, TRANSPORT_WEBRTC_DC_V1};
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

/// How often to send a WebSocket keepalive Ping on the control channel so an
/// idle middlebox (WS proxy / corp full-tunnel VPN) doesn't reap it after
/// ~5 min. Well under any real idle-reap window.
const WS_KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(30);

/// Auto-reconnect backoff bounds. Start short (a session that ran then dropped
/// usually reconnects instantly) and cap so a persistently-offline agent isn't
/// hammered.
const RECONNECT_BACKOFF_MIN: std::time::Duration = std::time::Duration::from_secs(1);
const RECONNECT_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// Cap on waiting for `rc:tunnel.quic.ready` after `rc:tunnel.opened`
/// negotiated `quic-v1`. The agent may walk several TURN-relay candidates
/// before replying — on a corp net that blocks UDP, a UDP attempt
/// (`:3478`, ~5 s) then a TURNS/TCP allocate (`:443`) — so this must cover
/// the agent's full tier walk, not just one RTT. 30 s comfortably bounds
/// 1–2 UDP timeouts + a TLS allocate. On timeout the client abandons QUIC
/// and (for `--transport auto`) re-opens over WebRTC.
const QUIC_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Phase 3d: head start we give the agent's TURN-permission install (it
/// fires when our `rc:tunnel.quic.candidate` reaches the agent over WS)
/// before we send the first QUIC Initial over the relay. QUIC's Initial
/// retransmission covers any remaining race, so this is just a latency
/// optimisation to avoid the first-packet drop + retransmit wait.
const QUIC_PERMIT_SETTLE: std::time::Duration = std::time::Duration::from_millis(300);

/// Operator's transport preference for `roomler-tunnel forward`,
/// selected with `--transport`. Drives which transports the client
/// advertises in `rc:tunnel.hello` and which it requests in
/// `rc:tunnel.open`; the server is authoritative for the final pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum TransportPref {
    /// Prefer QUIC; transparently fall back to WebRTC if QUIC setup
    /// fails. The default: server-side QUIC negotiation (Phase 1c) is
    /// deployed and gates on the agent's reported version, so QUIC is
    /// only attempted against agents that actually speak it — no wasted
    /// setup round-trip against an older agent.
    #[default]
    Auto,
    /// Force QUIC; error out if it can't be established (no fallback).
    Quic,
    /// Force the proven WebRTC SCTP DataChannel transport (the pre-QUIC
    /// default; still the right pick for a forced, no-fallback run).
    Webrtc,
}

impl TransportPref {
    /// Transports advertised in `rc:tunnel.hello`, in preference order.
    fn supported_transports(self) -> Vec<String> {
        match self {
            TransportPref::Auto => vec![
                TRANSPORT_QUIC_V1.to_string(),
                TRANSPORT_WEBRTC_DC_V1.to_string(),
            ],
            TransportPref::Quic => vec![TRANSPORT_QUIC_V1.to_string()],
            TransportPref::Webrtc => vec![TRANSPORT_WEBRTC_DC_V1.to_string()],
        }
    }

    /// The single transport requested in `rc:tunnel.open`.
    fn request_transport(self) -> &'static str {
        match self {
            TransportPref::Auto | TransportPref::Quic => TRANSPORT_QUIC_V1,
            TransportPref::Webrtc => TRANSPORT_WEBRTC_DC_V1,
        }
    }
}

/// Outcome of one [`run_session`] attempt, so `run` can decide whether
/// to fall back to WebRTC.
enum SessionOutcome {
    /// The session ran its data plane (listener loop entered).
    Completed,
    /// A QUIC session couldn't be established during setup; the caller
    /// may re-open over WebRTC.
    QuicSetupFailed,
}

/// Read half of the signaling WebSocket. Aliased so the per-transport
/// session bodies can own it without a generic bound (they move it into
/// a spawned dispatch task, which needs a concrete `Send + 'static`).
type WsSource = futures::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

/// Reply registry: per-flow oneshot for the server's accept/reject.
/// `pub(crate)` so the `udp` relay module shares the same correlation
/// map (flow_ids are unique per session across TCP + UDP).
pub(crate) type ReplyRegistry = Arc<Mutex<HashMap<u32, oneshot::Sender<ForwardReply>>>>;

/// Active-flow registry: which DC index a given flow is bound to, so
/// the WS dispatch can route inbound `TcpHalfClose` audit signals
/// (no demux action — in-band marker handles the data-plane close).
pub(crate) type ActiveFlows = Arc<Mutex<HashMap<u32, u8>>>;

#[derive(Debug)]
pub(crate) enum ForwardReply {
    Accept { dc_index: u8 },
    Reject { kind: RejectKind, reason: String },
}

/// Per-flow open round-trip cap, shared with the `udp` relay module.
pub(crate) const FLOW_OPEN_TIMEOUT_SHARED: std::time::Duration = FLOW_OPEN_TIMEOUT;

/// Entry point for `roomler-tunnel forward`. Tries the operator's
/// preferred transport and, for `--transport auto`, transparently
/// re-opens the session forcing `webrtc-dc-v1` if QUIC setup fails.
/// What each accepted local connection forwards to.
#[derive(Debug, Clone)]
pub enum Target {
    /// Static `--remote host:port` (the `forward` command) — every local
    /// connection dials the same destination.
    Static { host: String, port: u16 },
    /// Per-connection SOCKS5 CONNECT target (the `socks5` command) — the local
    /// port is a SOCKS5 proxy and each connection names its own destination.
    /// This is the tunnel's userspace mode: no OS routing, so it works on strict
    /// full-tunnel corp VPNs that capture the L3 overlay's routes.
    Socks5,
}

/// `roomler-tunnel forward` — one static local→remote TCP forward.
pub async fn run(
    cfg: TunnelConfig,
    agent_hex: &str,
    local: u16,
    remote: &str,
    transport: TransportPref,
) -> Result<()> {
    let (host, port) = parse_remote(remote)?;
    run_forward(
        cfg,
        agent_hex,
        local,
        Target::Static { host, port },
        transport,
    )
    .await
}

/// `roomler-tunnel socks5` — the userspace-mode SOCKS5 proxy. Same transport +
/// server policy + agent allowlist as a static forward; the destination is taken
/// from each connection's SOCKS5 CONNECT instead of a fixed `--remote`.
pub async fn run_socks5(
    cfg: TunnelConfig,
    agent_hex: &str,
    local: u16,
    transport: TransportPref,
) -> Result<()> {
    run_forward(cfg, agent_hex, local, Target::Socks5, transport).await
}

/// Shared driver for `forward` (static target) and `socks5` (per-connection
/// target). Runs sessions in an **auto-reconnect loop**: each session serves
/// local TCP connections until its WS control channel drops (idle-reap by a WS
/// proxy / corp VPN, a network blip, a VPN reconnect), then re-establishes with
/// backoff. Ctrl-C kills the process, which ends the loop.
async fn run_forward(
    cfg: TunnelConfig,
    agent_hex: &str,
    local: u16,
    target: Target,
    transport: TransportPref,
) -> Result<()> {
    let agent_id = ObjectId::parse_str(agent_hex)
        .with_context(|| format!("--agent must be a 24-hex ObjectId, got {agent_hex}"))?;

    info!(
        server = %cfg.server_url,
        agent = %agent_id,
        local,
        ?target,
        ?transport,
        "roomler-tunnel forward starting"
    );

    let mut backoff = RECONNECT_BACKOFF_MIN;
    loop {
        match run_one_session(&cfg, agent_id, local, &target, transport).await {
            // A session that established then dropped resets the backoff so the
            // reconnect is near-instant; a repeated setup failure grows it.
            Ok(()) => {
                info!("tunnel session ended; reconnecting");
                backoff = RECONNECT_BACKOFF_MIN;
            }
            Err(e) => {
                warn!(%e, backoff_s = backoff.as_secs(), "tunnel session failed; retrying");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
    }
}

/// One session attempt: request the preferred transport (Auto → QUIC →
/// WebRTC-DC fallback) and serve local TCP connections until the control
/// channel drops. Returns `Ok(())` when a session ran and ended (→ reconnect),
/// `Err` on a setup failure (→ backoff + retry).
async fn run_one_session(
    cfg: &TunnelConfig,
    agent_id: ObjectId,
    local: u16,
    target: &Target,
    transport: TransportPref,
) -> Result<()> {
    let outcome = run_session(
        cfg,
        agent_id,
        local,
        target,
        transport.supported_transports(),
        transport.request_transport(),
    )
    .await?;
    if matches!(outcome, SessionOutcome::QuicSetupFailed) {
        if transport == TransportPref::Auto {
            warn!("QUIC transport setup failed; re-opening session forcing webrtc-dc-v1");
            let fallback = run_session(
                cfg,
                agent_id,
                local,
                target,
                vec![TRANSPORT_WEBRTC_DC_V1.to_string()],
                TRANSPORT_WEBRTC_DC_V1,
            )
            .await?;
            if matches!(fallback, SessionOutcome::QuicSetupFailed) {
                bail!("webrtc-dc-v1 fallback unexpectedly reported QUIC-setup-failed");
            }
        } else {
            bail!("QUIC transport setup failed and --transport={transport:?} forbids fallback");
        }
    }
    Ok(())
}

/// One session attempt over a fresh WS connection: handshake, open the
/// tunnel requesting `request_transport`, then run whichever data plane
/// the server negotiated. Returns [`SessionOutcome::QuicSetupFailed`]
/// (not an `Err`) when a QUIC session can't be established, so the
/// caller can fall back to WebRTC.
#[allow(clippy::too_many_arguments)]
async fn run_session(
    cfg: &TunnelConfig,
    agent_id: ObjectId,
    local: u16,
    target: &Target,
    supported_transports: Vec<String>,
    request_transport: &str,
) -> Result<SessionOutcome> {
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
        // Keepalive so an idle middlebox (our nginx/HAProxy WS proxy, or a corp
        // full-tunnel VPN like Check Point) doesn't reap the control channel
        // after ~5 min of silence. Post-setup the data plane rides QUIC/DC and
        // the WS goes quiet, so without this the next SOCKS/forward connection —
        // which needs the WS to carry TcpForwardRequest — fails against a dead
        // socket. A protocol-level Ping needs no server change (axum auto-pongs).
        let mut keepalive = tokio::time::interval(WS_KEEPALIVE);
        keepalive.tick().await; // consume the immediate first tick
        loop {
            tokio::select! {
                maybe = outbound_rx.recv() => {
                    let Some(msg) = maybe else { break };
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
                _ = keepalive.tick() => {
                    if let Err(e) = ws_sink.send(Message::Ping(Vec::new().into())).await {
                        warn!(%e, "WS keepalive ping failed; sender exiting");
                        break;
                    }
                }
            }
        }
        debug!("outbound WS task exiting");
    });

    // Say hello — advertise every transport this client supports.
    outbound_tx
        .send(ClientMsg::TunnelHello {
            role: TunnelRole::Client,
            version: env!("CARGO_PKG_VERSION").to_string(),
            supported_transports,
        })
        .await
        .context("send TunnelHello")?;

    // Open the tunnel requesting our preferred transport; the server is
    // authoritative and echoes the negotiated one in `TunnelOpened`.
    outbound_tx
        .send(ClientMsg::TunnelOpen {
            agent_id,
            transport: request_transport.to_string(),
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
                    quic_auth_token,
                } => {
                    info!(
                        %session_id, %transport, dc_pool_size, sctp_rwnd_bytes,
                        ice_servers = ice_servers.len(),
                        quic = quic_auth_token.is_some(),
                        "rc:tunnel.opened"
                    );
                    break anyhow::Ok((session_id, transport, ice_servers, quic_auth_token));
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
    let (session_id, negotiated_transport, ice_servers, quic_auth_token) = opened;

    // ────────────── Dispatch on the negotiated transport ───────────
    if negotiated_transport == TRANSPORT_QUIC_V1 {
        return run_quic_session(
            ws_source,
            outbound_tx,
            session_id,
            quic_auth_token,
            ice_servers,
            local,
            target,
        )
        .await;
    }
    run_webrtc_session(
        ws_source,
        outbound_tx,
        session_id,
        ice_servers,
        local,
        target,
    )
    .await?;
    Ok(SessionOutcome::Completed)
}

/// The proven WebRTC SCTP DataChannel data plane (`webrtc-dc-v1`):
/// build the peer, run the SDP/ICE handshake, open the DC pool, then
/// serve local TCP connections over round-robin flows.
#[allow(clippy::too_many_arguments)]
async fn run_webrtc_session(
    mut ws_source: WsSource,
    outbound_tx: mpsc::Sender<ClientMsg>,
    session_id: ObjectId,
    ice_servers: Vec<IceServer>,
    local: u16,
    target: &Target,
) -> Result<()> {
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

    let mut dispatcher_task = {
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

    // Idle keepalive: webrtc-dc has no built-in keepalive (QUIC does), so
    // without periodic traffic an idle tunnel's TURN-relay permission /
    // NAT mapping lapses (~5 min) and the DTLS/SCTP association dies. Send
    // a tiny frame over dc(0); the agent mirrors it. Detached — it
    // self-exits when the pool drops at session end.
    if let Some(dc0) = peer_for_dispatch.dc(0) {
        tunnel_core::forward::spawn_dc_keepalive(dc0);
    }

    // ────────────── Local TCP listener ─────────────────────────────
    let listener = TcpListener::bind(("127.0.0.1", local))
        .await
        .with_context(|| format!("binding 127.0.0.1:{local}"))?;
    info!(local = %listener.local_addr()?, "listening for local TCP connections");

    let flow_counter = Arc::new(AtomicU32::new(1));
    let rr_counter = Arc::new(AtomicUsize::new(0));

    loop {
        let (mut tcp, peer_addr) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(x) => x,
                Err(e) => {
                    error!(%e, "accept failed");
                    continue;
                }
            },
            // The WS dispatcher exited — the control channel is gone, so new
            // flows can't be requested. End the session so `run_forward`
            // reconnects instead of accepting into a dead socket.
            _ = &mut dispatcher_task => {
                warn!("control channel closed; ending session to reconnect");
                break;
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

        let demuxes = Arc::clone(&demuxes);
        let reply_registry = Arc::clone(&reply_registry);
        let active_flows = Arc::clone(&active_flows);
        let outbound_tx = outbound_tx.clone();
        let target = target.clone();
        let flow_counter_for_udp = Arc::clone(&flow_counter);
        tokio::spawn(async move {
            // Resolve the destination: the static `--remote`, or the
            // per-connection SOCKS5 request (userspace mode). A SOCKS5
            // UDP ASSOCIATE forks off the UDP relay and never uses the
            // pre-allocated TCP flow_id.
            let (host, port, socks) = match &target {
                Target::Static { host, port } => (host.clone(), *port, false),
                Target::Socks5 => match crate::socks5::accept_request(&mut tcp).await {
                    Ok(crate::socks5::Socks5Request::Connect { host, port }) => (host, port, true),
                    Ok(crate::socks5::Socks5Request::UdpAssociate) => {
                        if let Err(e) = crate::udp::handle_associate(
                            tcp,
                            session_id,
                            crate::udp::AssocCarrier::Dc { demuxes },
                            reply_registry,
                            outbound_tx,
                            flow_counter_for_udp,
                        )
                        .await
                        {
                            warn!(%peer_addr, %e, "socks5 UDP associate ended with error");
                        }
                        return;
                    }
                    Err(e) => {
                        warn!(%peer_addr, %e, "socks5 handshake failed; dropping");
                        return;
                    }
                },
            };
            // Register the reply mailbox now that we're proceeding — before the
            // request is sent, so the dispatcher can route the accept/reject.
            let (reply_tx, reply_rx) = oneshot::channel::<ForwardReply>();
            reply_registry.lock().await.insert(flow_id, reply_tx);
            if let Err(e) = handle_local_connection(
                tcp,
                peer_addr,
                flow_id,
                dc_index_chosen,
                session_id,
                &host,
                port,
                outbound_tx,
                reply_rx,
                reply_registry,
                active_flows,
                demuxes,
                socks,
            )
            .await
            {
                warn!(flow_id, %e, "flow ended with error");
            }
        });
    }

    // Reached only when the dispatcher exited (control channel gone). Return so
    // `run_forward` reconnects; the `_sender_task` rides the WS teardown, and
    // in-flight per-flow tasks finish or die with the connection.
    Ok(())
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
    mut tcp: tokio::net::TcpStream,
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
    // SOCKS5 mode — send the CONNECT reply on this stream once the agent
    // accepts/rejects the forward (userspace mode); `false` for static forwards.
    socks: bool,
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
            if socks {
                crate::socks5::reply(&mut tcp, crate::socks5::REP_SUCCESS).await;
            }
            dc_index
        }
        ForwardReply::Reject { kind, reason } => {
            warn!(flow_id, ?kind, %reason, "rc:tunnel.tcp.reject — dropping local conn");
            if socks {
                crate::socks5::reply(&mut tcp, crate::socks5::REP_GENERAL_FAILURE).await;
            }
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

    let (from_dc, stats) = demux.register(flow_id).await;
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
    let close_reason = run_flow(tcp, dc, flow_id, from_dc, on_local_eof, stats).await;
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
            ServerMsg::UdpForwardAccept {
                session_id: sid,
                flow_id,
                dc_index,
            } if sid == session_id => {
                if let Some(tx) = reply_registry.lock().await.remove(&flow_id) {
                    let _ = tx.send(ForwardReply::Accept { dc_index });
                } else {
                    warn!(flow_id, "udp accept for unknown flow_id");
                }
            }
            ServerMsg::UdpForwardReject {
                session_id: sid,
                flow_id,
                kind,
                reason,
            } if sid == session_id => {
                if let Some(tx) = reply_registry.lock().await.remove(&flow_id) {
                    let _ = tx.send(ForwardReply::Reject { kind, reason });
                } else {
                    warn!(flow_id, ?kind, %reason, "udp reject for unknown flow_id");
                }
            }
            ServerMsg::UdpClosed {
                session_id: sid,
                flow_id,
                reason,
            } if sid == session_id => {
                debug!(flow_id, ?reason, "rc:tunnel.udp.closed (audit)");
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

/// The QUIC data plane (`quic-v1`). Awaits the agent's
/// `rc:tunnel.quic.ready` (relayed by the server), connects to the
/// agent's quinn endpoint (cert pinned from that message, authed with
/// the server-minted token), then serves local TCP connections — one
/// QUIC bidirectional stream per flow. Returns
/// [`SessionOutcome::QuicSetupFailed`] (not an `Err`) if the QUIC link
/// can't be established during setup, so the caller can fall back to
/// WebRTC. Once flows can start it's committed (the listener loop runs
/// until process teardown, like the WebRTC path).
#[allow(clippy::too_many_arguments)]
async fn run_quic_session(
    mut ws_source: WsSource,
    outbound_tx: mpsc::Sender<ClientMsg>,
    session_id: ObjectId,
    quic_auth_token: Option<String>,
    ice_servers: Vec<IceServer>,
    local: u16,
    target: &Target,
) -> Result<SessionOutcome> {
    let Some(token) = quic_auth_token else {
        warn!("server negotiated quic-v1 but sent no quic_auth_token — cannot authenticate");
        return Ok(SessionOutcome::QuicSetupFailed);
    };

    // Await `rc:tunnel.quic.ready`: the agent's ephemeral cert
    // fingerprint to pin + the dialable addrs.
    let ready = tokio::time::timeout(QUIC_READY_TIMEOUT, async {
        loop {
            match recv_server_msg(&mut ws_source).await? {
                ServerMsg::TunnelQuicReady {
                    session_id: sid,
                    cert_fingerprint,
                    addrs,
                } if sid == session_id => break anyhow::Ok((cert_fingerprint, addrs)),
                ServerMsg::TunnelRevoked { reason } => {
                    bail!("tunnel revoked during quic setup: {reason}")
                }
                ServerMsg::TunnelTerminate { reason, .. } => {
                    bail!("tunnel terminated during quic setup: {reason:?}")
                }
                other => debug!(?other, "ignoring pre-quic-ready ServerMsg"),
            }
        }
    })
    .await;
    let (cert_fingerprint, addrs) = match ready {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            warn!(%e, "error awaiting rc:tunnel.quic.ready");
            return Ok(SessionOutcome::QuicSetupFailed);
        }
        Err(_) => {
            warn!("timed out waiting for rc:tunnel.quic.ready");
            return Ok(SessionOutcome::QuicSetupFailed);
        }
    };
    info!(
        addrs = ?addrs,
        fp_prefix = %cert_fingerprint.chars().take(12).collect::<String>(),
        "rc:tunnel.quic.ready"
    );

    // Establish the QUIC connection. Phase 3d: when the server minted
    // coturn creds we ride QUIC-over-TURN (Tier 2) — the agent advertised
    // its relay address, and we dial it through our OWN relay so coturn's
    // permission model lets the datagrams flow. Otherwise (no creds) dial
    // the agent's direct host candidates (Phase 1e/2a). Either branch
    // yields a connected `(peer, conn)`; auth + the data plane below are
    // transport-agnostic.
    let (peer, conn, path) = if let Some((urls, user, cred)) = pick_turn_creds(&ice_servers) {
        info!("QUIC: server provided TURN creds — establishing QUIC-over-TURN (relay)");
        match setup_quic_over_relay(
            &urls,
            &user,
            &cred,
            &cert_fingerprint,
            &addrs,
            session_id,
            &outbound_tx,
        )
        .await
        {
            // Relay sub-tier — UDP (Tier 2) vs TURNS/TCP (Tier 3) — is
            // logged by the allocation itself ("TURN allocation established"
            // vs "TURNS/TCP …"); at the QUIC level both are the relay path.
            Some((peer, conn)) => (peer, conn, "relay"),
            None => return Ok(SessionOutcome::QuicSetupFailed),
        }
    } else {
        let bind: std::net::SocketAddr =
            "0.0.0.0:0".parse().expect("0.0.0.0:0 is a valid bind addr");
        let peer = match QuicPeer::client(bind, &cert_fingerprint) {
            Ok(p) => p,
            Err(e) => {
                warn!(%e, "QuicPeer::client failed");
                return Ok(SessionOutcome::QuicSetupFailed);
            }
        };
        // Dial the advertised addrs in priority order (direct host /
        // srflx candidates).
        let Some(conn) = connect_first(&peer, &addrs).await else {
            warn!(addrs = ?addrs, "could not connect QUIC to any advertised addr");
            return Ok(SessionOutcome::QuicSetupFailed);
        };
        (peer, conn, "direct")
    };
    if let Err(e) = quic::client_authenticate(&conn, &token).await {
        warn!(%e, "QUIC client_authenticate failed");
        return Ok(SessionOutcome::QuicSetupFailed);
    }
    // Per-tier connection summary — one greppable line for field
    // diagnosis: transport + path (relay vs direct hole-punch) + the
    // negotiated peer address. The relay sub-tier (UDP Tier 2 / TURNS-TCP
    // Tier 3) and our own relay address are in the adjacent
    // relay-allocation log lines; throughput follows in the 2 s logger.
    info!(
        transport = "quic-v1",
        path,
        remote = %conn.remote_address(),
        "tunnel established"
    );

    // From here the QUIC link is live; we're committed (no WebRTC
    // fallback once flows can start). Spawn the WS dispatcher for
    // per-flow accept/reject + teardown signals.
    let reply_registry: ReplyRegistry = Arc::new(Mutex::new(HashMap::new()));
    let active_flows: ActiveFlows = Arc::new(Mutex::new(HashMap::new()));
    let mut dispatcher_task = {
        let reply_registry = Arc::clone(&reply_registry);
        let active_flows = Arc::clone(&active_flows);
        let outbound_tx = outbound_tx.clone();
        tokio::spawn(async move {
            quic_dispatch_loop(
                ws_source,
                session_id,
                reply_registry,
                active_flows,
                outbound_tx,
            )
            .await
        })
    };

    // Keep the endpoint + connection alive for the session lifetime
    // (dropping the endpoint closes quinn; dropping the last `conn`
    // Arc closes the connection).
    let conn = Arc::new(conn);
    let _peer = Arc::new(peer);

    // ────────────── Local TCP listener ─────────────────────────────
    let listener = TcpListener::bind(("127.0.0.1", local))
        .await
        .with_context(|| format!("binding 127.0.0.1:{local}"))?;
    info!(local = %listener.local_addr()?, "listening for local TCP connections (quic-v1)");
    let flow_counter = Arc::new(AtomicU32::new(1));

    loop {
        let (mut tcp, peer_addr) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(x) => x,
                Err(e) => {
                    error!(%e, "accept failed");
                    continue;
                }
            },
            // WS dispatcher exited (control channel gone) — end the session so
            // `run_forward` reconnects instead of accepting into a dead socket.
            _ = &mut dispatcher_task => {
                warn!("control channel closed; ending quic session to reconnect");
                break;
            }
        };
        // Same Nagle + SO_SNDBUF tuning as the WebRTC path — see the
        // long-form rationale in `run_webrtc_session`'s listener loop.
        if let Err(e) = tcp.set_nodelay(true) {
            warn!(%peer_addr, %e, "set_nodelay(true) on local TCP failed");
        }
        #[cfg(any(unix, windows))]
        {
            use socket2::SockRef;
            const LOCAL_SNDBUF_BYTES: usize = 4 * 1024 * 1024;
            let sock = SockRef::from(&tcp);
            if let Err(e) = sock.set_send_buffer_size(LOCAL_SNDBUF_BYTES) {
                warn!(%peer_addr, %e, "set_send_buffer_size(4MiB) on local TCP failed");
            }
        }
        debug!(%peer_addr, "accepted local TCP connection (quic-v1)");

        let flow_id = flow_counter.fetch_add(1, Ordering::Relaxed);

        let reply_registry = Arc::clone(&reply_registry);
        let active_flows = Arc::clone(&active_flows);
        let outbound_tx = outbound_tx.clone();
        let target = target.clone();
        let conn = Arc::clone(&conn);
        let flow_counter_for_udp = Arc::clone(&flow_counter);
        tokio::spawn(async move {
            // Resolve the destination: static `--remote`, or the per-connection
            // SOCKS5 request (userspace mode). UDP ASSOCIATE forks off the UDP
            // relay over this session's QUIC connection.
            let (host, port, socks) = match &target {
                Target::Static { host, port } => (host.clone(), *port, false),
                Target::Socks5 => match crate::socks5::accept_request(&mut tcp).await {
                    Ok(crate::socks5::Socks5Request::Connect { host, port }) => (host, port, true),
                    Ok(crate::socks5::Socks5Request::UdpAssociate) => {
                        if let Err(e) = crate::udp::handle_associate(
                            tcp,
                            session_id,
                            crate::udp::AssocCarrier::Quic { conn },
                            reply_registry,
                            outbound_tx,
                            flow_counter_for_udp,
                        )
                        .await
                        {
                            warn!(%peer_addr, %e, "socks5 UDP associate ended with error");
                        }
                        return;
                    }
                    Err(e) => {
                        warn!(%peer_addr, %e, "socks5 handshake failed; dropping");
                        return;
                    }
                },
            };
            let (reply_tx, reply_rx) = oneshot::channel::<ForwardReply>();
            reply_registry.lock().await.insert(flow_id, reply_tx);
            if let Err(e) = handle_local_connection_quic(
                tcp,
                peer_addr,
                flow_id,
                session_id,
                conn,
                &host,
                port,
                outbound_tx,
                reply_rx,
                reply_registry,
                active_flows,
                socks,
            )
            .await
            {
                warn!(flow_id, %e, "quic flow ended with error");
            }
        });
    }

    // Reached only when the dispatcher exited — return so `run_forward`
    // reconnects (the QUIC endpoint + conn drop here, closing the connection).
    Ok(SessionOutcome::Completed)
}

/// Try each advertised addr in order; return the first QUIC connection
/// that handshakes. Logs + skips unparseable / unreachable addrs.
async fn connect_first(peer: &QuicPeer, addrs: &[String]) -> Option<QuicConnection> {
    for a in addrs {
        let Ok(sa) = a.parse::<std::net::SocketAddr>() else {
            warn!(addr = %a, "skipping unparseable quic addr");
            continue;
        };
        match peer.connect(sa).await {
            Ok(c) => return Some(c),
            Err(e) => warn!(addr = %sa, %e, "quic connect failed; trying next addr"),
        }
    }
    None
}

/// Pick the first ICE server carrying usable plain-UDP TURN relay creds
/// (a `turn:…?transport=udp` url plus username + credential). Returns the
/// `(urls, username, credential)` for [`setup_quic_over_relay`], or
/// `None` when the server sent only STUN / TLS-TCP entries (→ direct
/// QUIC). Phase 3d.
fn pick_turn_creds(ice_servers: &[IceServer]) -> Option<(Vec<String>, String, String)> {
    ice_servers
        .iter()
        .find_map(|s| match (&s.username, &s.credential) {
            (Some(u), Some(c)) if relay::turn_udp_server(&s.urls).is_some() => {
                Some((s.urls.clone(), u.clone(), c.clone()))
            }
            _ => None,
        })
}

/// Phase 3d: bring the client's QUIC endpoint up over its OWN coturn TURN
/// relay and dial the agent's relay address (QUIC-over-TURN, Tier 2).
///
/// 1. Allocate a relay from the session creds → a [`relay::RelayUdpSocket`]
///    quinn rides; the relayed address is what coturn handed us.
/// 2. Send `rc:tunnel.quic.candidate { our relay addr }` so the agent
///    installs a TURN permission for us (it's the QUIC server + never
///    sends first).
/// 3. Bootstrap our OWN permission for each agent relay addr (one stray
///    datagram each — the webrtc-rs TURN client auto-creates the
///    CreatePermission on first send; the agent's quinn discards the
///    byte). This is the mutual half coturn needs to relay the agent's
///    handshake replies back to us.
/// 4. After a short settle, dial the agent's relay addr over our relay.
///
/// Returns the connected `(peer, conn)` or `None` on any setup failure
/// (caller soft-falls back to webrtc-dc-v1). The full Tier-2 datagram +
/// permission path is proven in tunnel-core's
/// `quinn_runs_over_two_turn_allocations`.
#[allow(clippy::too_many_arguments)]
async fn setup_quic_over_relay(
    urls: &[String],
    username: &str,
    credential: &str,
    cert_fingerprint: &str,
    agent_addrs: &[String],
    session_id: ObjectId,
    outbound_tx: &mpsc::Sender<ClientMsg>,
) -> Option<(QuicPeer, QuicConnection)> {
    // Same-worker pin: coturn relays between two allocations on the SAME
    // worker via hairpin, but cross-worker relay-to-relay breaks on this
    // cluster (the dual-public-IP SNAT rewrites the relay's egress source
    // so the peer's CreatePermission no longer matches). The agent — often
    // UDP-blocked — can't be pinned (its TLS allocate needs the coturn
    // hostname for SNI), so the CLIENT follows the agent onto its worker by
    // allocating its UDP relay directly on the agent's relay IP. Falls back
    // to the round-robin hostname urls if that UDP allocate fails (e.g. a
    // UDP-blocked controller), which lands cross-worker but at least tries.
    let mut alloc_urls: Vec<String> = Vec::new();
    if let Some(ip) = agent_addrs
        .iter()
        .find_map(|a| a.parse::<std::net::SocketAddr>().ok().map(|s| s.ip()))
    {
        let host = if ip.is_ipv6() {
            format!("[{ip}]")
        } else {
            ip.to_string()
        };
        alloc_urls.push(format!("turn:{host}:3478?transport=udp"));
        info!(%ip, "QUIC client: pinning relay to the agent's coturn worker (hairpin)");
    }
    alloc_urls.extend_from_slice(urls);
    let turn_relay = match relay::allocate_relay_from_ice(&alloc_urls, username, credential).await {
        Ok(r) => r,
        Err(e) => {
            warn!(%e, "QUIC client: TURN allocate failed");
            return None;
        }
    };
    let relay_conn: Arc<dyn relay::RelayConn> = Arc::new(turn_relay);
    let our_relay_addr = match relay_conn.local_addr() {
        Ok(a) => a,
        Err(e) => {
            warn!(%e, "QUIC client: relay local_addr");
            return None;
        }
    };
    info!(relay_addr = %our_relay_addr, "QUIC client: TURN relay allocated");

    let sock = match relay::RelayUdpSocket::new(Arc::clone(&relay_conn)) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            warn!(%e, "QUIC client: relay socket bridge");
            return None;
        }
    };
    let peer = match QuicPeer::client_over_abstract_socket(sock, cert_fingerprint) {
        Ok(p) => p,
        Err(e) => {
            warn!(%e, "QUIC client: endpoint over relay");
            return None;
        }
    };

    // Tell the agent our relay addr so it permits us (server relays this
    // candidate to the agent, which installs the TURN permission).
    if let Err(e) = outbound_tx
        .send(ClientMsg::TunnelQuicCandidate {
            session_id,
            addrs: vec![our_relay_addr.to_string()],
        })
        .await
    {
        warn!(%e, "QUIC client: send relay candidate failed");
        return None;
    }
    // Bootstrap our side of the mutual permission for each agent relay
    // addr.
    for a in agent_addrs {
        if let Ok(sa) = a.parse::<std::net::SocketAddr>()
            && let Err(e) = relay_conn.send_to(b"\x00", sa).await
        {
            debug!(addr = %sa, %e, "QUIC client: permission bootstrap datagram failed");
        }
    }
    tokio::time::sleep(QUIC_PERMIT_SETTLE).await;

    match connect_first(&peer, agent_addrs).await {
        Some(conn) => Some((peer, conn)),
        None => {
            warn!(addrs = ?agent_addrs, "QUIC client: could not connect over the relay");
            None
        }
    }
}

/// WS read loop for a QUIC session. Routes per-flow accept/reject into
/// the `reply_registry` and handles teardown. Unlike the WebRTC
/// [`dispatch_loop`] it has no SDP/ICE to forward — QUIC carries its
/// own handshake — so it only consumes the control-plane signals.
async fn quic_dispatch_loop(
    mut ws_source: WsSource,
    session_id: ObjectId,
    reply_registry: ReplyRegistry,
    active_flows: ActiveFlows,
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
                debug!(len = d.len(), "ws ping");
                continue;
            }
            Ok(_) => continue,
            Err(e) => {
                warn!(%e, "ws read error; exiting quic dispatch loop");
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
            ServerMsg::UdpForwardAccept {
                session_id: sid,
                flow_id,
                dc_index,
            } if sid == session_id => {
                if let Some(tx) = reply_registry.lock().await.remove(&flow_id) {
                    let _ = tx.send(ForwardReply::Accept { dc_index });
                } else {
                    warn!(flow_id, "udp accept for unknown flow_id");
                }
            }
            ServerMsg::UdpForwardReject {
                session_id: sid,
                flow_id,
                kind,
                reason,
            } if sid == session_id => {
                if let Some(tx) = reply_registry.lock().await.remove(&flow_id) {
                    let _ = tx.send(ForwardReply::Reject { kind, reason });
                } else {
                    warn!(flow_id, ?kind, %reason, "udp reject for unknown flow_id");
                }
            }
            ServerMsg::UdpClosed {
                session_id: sid,
                flow_id,
                reason,
            } if sid == session_id => {
                debug!(flow_id, ?reason, "rc:tunnel.udp.closed (audit)");
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
            other => debug!(?other, "quic dispatch: ignoring ServerMsg"),
        }
    }
    debug!("WS source ended; quic dispatch loop exiting");
}

/// QUIC analogue of [`handle_local_connection`]: request the forward,
/// await accept/reject, then open a QUIC bidirectional stream for the
/// flow and pump it with [`run_flow_quic`]. No DC pool / round-robin —
/// each flow is its own stream.
#[allow(clippy::too_many_arguments)]
async fn handle_local_connection_quic(
    mut tcp: tokio::net::TcpStream,
    peer_addr: std::net::SocketAddr,
    flow_id: u32,
    session_id: ObjectId,
    conn: Arc<QuicConnection>,
    dst_host: &str,
    dst_port: u16,
    outbound_tx: mpsc::Sender<ClientMsg>,
    reply_rx: oneshot::Receiver<ForwardReply>,
    reply_registry: ReplyRegistry,
    active_flows: ActiveFlows,
    socks: bool,
) -> Result<()> {
    // Request the forward.
    outbound_tx
        .send(ClientMsg::TcpForwardRequest {
            session_id,
            flow_id,
            dst_host: dst_host.to_string(),
            dst_port,
        })
        .await
        .context("send TcpForwardRequest")?;

    // Await accept/reject.
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
    match reply {
        ForwardReply::Accept { dc_index } => {
            // dc_index is meaningless for QUIC (the agent sends 0);
            // logged only for symmetry with the WebRTC path.
            debug!(flow_id, dc_index, "rc:tunnel.tcp.accept (quic)");
            if socks {
                crate::socks5::reply(&mut tcp, crate::socks5::REP_SUCCESS).await;
            }
        }
        ForwardReply::Reject { kind, reason } => {
            warn!(flow_id, ?kind, %reason, "rc:tunnel.tcp.reject — dropping local conn");
            if socks {
                crate::socks5::reply(&mut tcp, crate::socks5::REP_GENERAL_FAILURE).await;
            }
            drop(tcp);
            return Ok(());
        }
    }

    active_flows.lock().await.insert(flow_id, 0);
    // Open the QUIC stream for this flow (writes the 4-byte flow_id
    // preamble the agent reads to correlate the stream to the dialed
    // dst via its `take_flow` rendezvous).
    let (send, recv) = match quic::open_flow(&conn, flow_id).await {
        Ok(s) => s,
        Err(e) => {
            active_flows.lock().await.remove(&flow_id);
            drop(tcp);
            bail!("quic open_flow for flow {flow_id}: {e}");
        }
    };

    let stats = Arc::new(tunnel_core::forward::FlowStats::default());
    debug!(flow_id, %peer_addr, "running quic flow");
    let close_reason = run_flow_quic(tcp, send, recv, flow_id, stats).await;
    info!(flow_id, ?close_reason, "quic flow ended");
    let _ = outbound_tx
        .send(ClientMsg::TcpClosed {
            session_id,
            flow_id,
            reason: close_reason,
        })
        .await;
    active_flows.lock().await.remove(&flow_id);
    Ok(())
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

    /// Phase 3d: `pick_turn_creds` must select the TURN ICE server (with
    /// usable UDP creds) out of the production list — which leads with a
    /// credential-less STUN entry — and ignore a creds-less or
    /// TLS/TCP-only list (→ direct QUIC, no relay).
    #[test]
    fn pick_turn_creds_selects_the_udp_turn_server() {
        let ice = vec![
            IceServer {
                urls: vec!["stun:stun.l.google.com:19302".into()],
                username: None,
                credential: None,
            },
            IceServer {
                urls: vec![
                    "turn:coturn.roomler.ai:3478?transport=udp".into(),
                    "turns:coturn.roomler.ai:5349?transport=tcp".into(),
                ],
                username: Some("1780000000:sess".into()),
                credential: Some("base64hmac".into()),
            },
        ];
        let (urls, user, cred) = pick_turn_creds(&ice).expect("must find the TURN server");
        assert!(urls.iter().any(|u| u.starts_with("turn:")));
        assert_eq!(user, "1780000000:sess");
        assert_eq!(cred, "base64hmac");

        // STUN-only → no relay creds.
        let stun_only = vec![IceServer {
            urls: vec!["stun:stun.l.google.com:19302".into()],
            username: None,
            credential: None,
        }];
        assert!(pick_turn_creds(&stun_only).is_none());

        // TURN url present but no creds → unusable.
        let no_creds = vec![IceServer {
            urls: vec!["turn:coturn.roomler.ai:3478?transport=udp".into()],
            username: None,
            credential: None,
        }];
        assert!(pick_turn_creds(&no_creds).is_none());
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

    #[test]
    fn transport_pref_advertises_and_requests_correctly() {
        // Locks the exact wire strings transport negotiation depends on
        // (catches accidental drift in the tunnel_core consts).
        assert_eq!(
            TransportPref::Webrtc.supported_transports(),
            vec!["webrtc-dc-v1".to_string()]
        );
        assert_eq!(TransportPref::Webrtc.request_transport(), "webrtc-dc-v1");

        assert_eq!(
            TransportPref::Quic.supported_transports(),
            vec!["quic-v1".to_string()]
        );
        assert_eq!(TransportPref::Quic.request_transport(), "quic-v1");

        // Auto advertises BOTH (quic first = preference order) + requests quic.
        assert_eq!(
            TransportPref::Auto.supported_transports(),
            vec!["quic-v1".to_string(), "webrtc-dc-v1".to_string()]
        );
        assert_eq!(TransportPref::Auto.request_transport(), "quic-v1");

        // Default is Auto now that server-side QUIC negotiation (Phase
        // 1c) is deployed: prefer QUIC, fall back to WebRTC on failure.
        assert_eq!(TransportPref::default(), TransportPref::Auto);
    }

    /// Client glue: [`handle_local_connection_quic`] sends the forward
    /// request, on `Accept` opens a QUIC flow, and `run_flow_quic` pumps
    /// the local TCP socket to the agent and back. The "agent" here is
    /// built from tunnel-core primitives (server endpoint + auth +
    /// accept_flow + run_flow_quic to a loopback echo dst) — the
    /// symmetric counterpart to the agent crate's
    /// `handle_forward_request_quic` test.
    #[tokio::test(flavor = "multi_thread")]
    async fn quic_local_connection_requests_accepts_and_pumps() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Loopback TCP echo "dst" the agent dials.
        let dst = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dst_port = dst.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = dst.accept().await.unwrap();
            let (mut r, mut w) = s.split();
            let _ = tokio::io::copy(&mut r, &mut w).await;
        });

        // "Agent": quinn server endpoint that authenticates the client,
        // accepts one flow, dials the echo dst, and pumps it.
        let (agent, fp) = QuicPeer::server("127.0.0.1:0".parse().unwrap()).unwrap();
        let agent_addr = agent.local_addr().unwrap();
        let token = "client-glue-token".to_string();
        let token_a = token.clone();
        tokio::spawn(async move {
            let conn = agent.accept().await.unwrap().unwrap();
            quic::server_authenticate(&conn, &token_a).await.unwrap();
            let (flow_id, send, recv) = quic::accept_flow(&conn).await.unwrap();
            let dst_tcp = tokio::net::TcpStream::connect(("127.0.0.1", dst_port))
                .await
                .unwrap();
            let stats = Arc::new(tunnel_core::forward::FlowStats::default());
            tunnel_core::forward::run_flow_quic(dst_tcp, send, recv, flow_id, stats).await;
        });

        // Client: connect + authenticate (cert pinned to the agent's fp).
        let client = QuicPeer::client("127.0.0.1:0".parse().unwrap(), &fp).unwrap();
        let conn = Arc::new(client.connect(agent_addr).await.unwrap());
        quic::client_authenticate(&conn, &token).await.unwrap();

        // Local app socket <-> the `tcp` we hand to the glue.
        let local_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = local_listener.local_addr().unwrap();
        let app = tokio::net::TcpStream::connect(local_addr).await.unwrap();
        let (tcp, _) = local_listener.accept().await.unwrap();

        // Pre-arm the reply oneshot with Accept (in production the WS
        // dispatcher fills this from the server's TcpForwardAccept).
        let (reply_tx, reply_rx) = oneshot::channel::<ForwardReply>();
        reply_tx.send(ForwardReply::Accept { dc_index: 0 }).unwrap();

        let (outbound_tx, mut outbound_rx) = mpsc::channel::<ClientMsg>(16);
        let reply_registry: ReplyRegistry = Arc::new(Mutex::new(HashMap::new()));
        let active_flows: ActiveFlows = Arc::new(Mutex::new(HashMap::new()));
        let session_id = ObjectId::new();
        let flow_id = 1u32;

        let conn_c = Arc::clone(&conn);
        let glue = tokio::spawn(async move {
            handle_local_connection_quic(
                tcp,
                "127.0.0.1:0".parse().unwrap(),
                flow_id,
                session_id,
                conn_c,
                "echo.intranet",
                dst_port,
                outbound_tx,
                reply_rx,
                reply_registry,
                active_flows,
                false,
            )
            .await
        });

        // The glue sends a TcpForwardRequest first.
        match outbound_rx
            .recv()
            .await
            .expect("expected TcpForwardRequest from glue")
        {
            ClientMsg::TcpForwardRequest {
                flow_id: f,
                dst_port: p,
                ..
            } => {
                assert_eq!(f, flow_id);
                assert_eq!(p, dst_port);
            }
            other => panic!("expected TcpForwardRequest, got {other:?}"),
        }

        // Local app writes, half-closes; expects the echo back over QUIC.
        let (mut app_r, mut app_w) = app.into_split();
        app_w.write_all(b"ping over client quic").await.unwrap();
        app_w.shutdown().await.unwrap();
        let mut echoed = Vec::new();
        app_r.read_to_end(&mut echoed).await.unwrap();
        assert_eq!(
            &echoed, b"ping over client quic",
            "bytes must round-trip the local TCP ↔ QUIC ↔ agent ↔ dst loop"
        );

        glue.await.unwrap().expect("glue returns Ok");
        // After the flow ends the glue emits TcpClosed for audit.
        match outbound_rx
            .recv()
            .await
            .expect("expected TcpClosed after flow end")
        {
            ClientMsg::TcpClosed { flow_id: f, .. } => assert_eq!(f, flow_id),
            other => panic!("expected TcpClosed, got {other:?}"),
        }
    }
}

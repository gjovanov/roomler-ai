// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! WebSocket handler for `roomler` clients (`role=tunnel-client`).
//!
//! T2.5 wires the server-side ACL gate. Lifecycle:
//!
//! 1. Verify the TunnelClient JWT at WS upgrade (`ws::handler::
//!    ws_upgrade_tunnel_client`) and run a connect-time revocation
//!    check on the row.
//! 2. Drive the WS read loop: parse each text frame as
//!    `signaling::ClientMsg`, dispatch on the `rc:tunnel.*` variants:
//!    - `TunnelHello`: cap-negotiation handshake.
//!    - `TunnelOpen`: cross-tenant gate (defence-in-depth) + audit
//!      the session start. T2.6+ will add the actual WebRTC offer/
//!      answer + DC-pool negotiation; for now we reply with a
//!      `TunnelOpened` stub so downstream tests can drive the
//!      dispatch surface.
//!    - `TcpForwardRequest`: invoke
//!      `tunnel_core::policy::check_forward_request` against the
//!      live `tunnel_policies` for the agent's tenant. Audit either
//!      `TcpAccept` or `TcpReject`. T2.6+ relays accepts to the
//!      agent's WS; today we reply with a stub `TcpForwardAccept`
//!      that downstream tests assert on.
//!    - Other variants land in T2.6+ (data-plane relays).
//! 3. Periodic revocation re-check (60 s) — re-reads `tunnel_clients.
//!    status`, sends `TunnelRevoked` if the row turns bad mid-session.
//! 4. Keepalive (#4571) — a Ping every 30 s from the write side; a socket
//!    that delivers no frame for 90 s ends as a lost link does.
//!
//! Audit context: every `tunnel forward` invocation gets one
//! `tunnel_session_id` (assigned by the server on `TunnelOpen`).
//! Every flow + peer event references it for correlation in the
//! audit log (see `crates/services/src/dao/tunnel_audit.rs`).

use axum::extract::ws::{Message, WebSocket};
use bson::{DateTime, oid::ObjectId};
use futures::{SinkExt, StreamExt, stream::SplitSink};
use roomler_ai_remote_control::{
    models::{AgentStatus, OsKind, RelayMode, TunnelAuditEvent, TunnelAuditKind},
    signaling::{
        ClientMsg, CloseReason, NET_GRANT_REFUSED, REJECT_REASON_NO_SESSION,
        REJECT_REASON_SESSION_MISMATCH, RejectKind, ServerMsg,
    },
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{debug, info, warn};
use tunnel_core::policy::{
    GateResult, PolicySubject, Principal, ProtocolKind, ResolvedSubject, TunnelPolicy,
    check_forward_request,
};
use tunnel_core::transport::{TRANSPORT_QUIC_V1, TRANSPORT_WEBRTC_DC_V1};

use crate::NetworkState;
use crate::agent_arms::pump_server_messages;
use axum::{extract::WebSocketUpgrade, response::Response};
use roomler_ai_mod_fleet::agent::{TunnelClientSeat, TunnelClientSockets};
use roomler_core::cluster::directory::{
    OwnershipDirectory, TUNNEL_CLIENT_CHECK_SECS, TUNNEL_CLIENT_TTL_SECS, tunnel_client_key,
};
use roomler_core::ws::upgrade::{MAX_WS_MESSAGE_BYTES, send_goodbye_and_close, tid_matches_claim};

/// Outbound channel capacity. Per-tunnel-client. Generous because a
/// single peer multiplexes many flows; the dominant traffic is per-
/// flow audit relays + ICE trickle, both small and sporadic.
const OUTBOUND_CAP: usize = 256;

/// Cadence of the revocation re-check. 60 s is the v1 default —
/// cheap (one Mongo find_by_id per minute per active connection)
/// and matches the existing agent heartbeat rhythm. #4553: the same check
/// refreshes the client's presence record and its `last_seen_at` trail, so
/// the period is the directory's, which sets the record's TTL from it.
const REVOCATION_CHECK_INTERVAL: Duration = Duration::from_secs(TUNNEL_CLIENT_CHECK_SECS);

/// #4571 — the server's keepalive on a tunnel client's socket: a Ping every
/// period, sent from the write side ([`spawn_keepalive`]). 30 s is the
/// client's own keepalive (`roomler`'s `WS_KEEPALIVE`) and the DERP socket's
/// (`DERP_KEEPALIVE`, whose note records the 300 s idle timeout of the proxy
/// in front of the pods): ten periods inside it.
const TUNNEL_PING_EVERY: Duration = Duration::from_secs(30);

/// #4571 — how many keepalive periods with no frame from the client end its
/// socket: 90 s, the client's own receive deadline (`roomler`'s
/// `WS_RX_DEADLINE`), so each side gives a dead link up in the same window.
/// A healthy client delivers a frame every period (it answers each Ping while
/// it reads, and pings on its own since tunnel-v0.3.0-rc.148), so a late or
/// lost frame never ends it, and one that vanished reads offline 90 s after
/// its last frame.
const TUNNEL_SILENT_PERIODS: u32 = 3;

/// The keepalive period and the receive deadline a tunnel client's socket
/// runs: [`TUNNEL_PING_EVERY`] and [`TUNNEL_SILENT_PERIODS`] of it, or a
/// test's period (`rc.tunnel_ping_every`, which no deployment can set).
fn keepalive(rc: &roomler_ai_config::RcSettings) -> (Duration, Duration) {
    let every = rc.tunnel_ping_every.unwrap_or(TUNNEL_PING_EVERY);
    (every, every * TUNNEL_SILENT_PERIODS)
}

/// How long the check waits for the connection loop to have told the
/// session's exit that the connection ends, before it tells the client
/// either way ([`signal_end`]).
const END_TOLD_WITHIN: Duration = Duration::from_secs(5);

/// Drive a tunnel-client WS until either the client closes, the
/// signalling layer errors, or the revocation re-check kills the
/// connection. Replaces the T1 stub message loop with real
/// `rc:tunnel.*` dispatch.
pub async fn handle_tunnel_client_socket(
    state: NetworkState,
    socket: WebSocket,
    tunnel_client_id: ObjectId,
    tenant_id: ObjectId,
    owner_user_id: ObjectId,
    dialed_tid: Option<String>,
) {
    info!(%tunnel_client_id, %tenant_id, %owner_user_id, "tunnel-client WS connected");
    let conn_established_ms = bson::DateTime::now().timestamp_millis();

    // #4553 — presence. The trail first: `last_seen_at` is otherwise written
    // only by the 60 s check, whose first run is a full period away, and a
    // pod that cannot reach the directory reads the trail alone. Then this
    // socket's seat on this pod and the cross-pod record, both before the
    // check that refreshes the record starts.
    if let Err(e) = state.tunnel_clients.touch_heartbeat(tunnel_client_id).await {
        debug!(%tunnel_client_id, %e, "tunnel-client connect: last_seen_at write failed");
    }
    let (presence_seat, presence) = PresenceRecord::open(&state, tunnel_client_id).await;

    let (socket_tx, mut socket_rx) = socket.split();
    let socket_tx = Arc::new(Mutex::new(socket_tx));

    // Outbound channel pattern — mirrors the agent WS handler's
    // Hub-owned mpsc. All `ServerMsg` writes go through `outbound_tx`;
    // a single pump task drains the receiver onto the socket. Once
    // the tunnel-session is established we register a clone of
    // `outbound_tx` in `NetworkState::tunnel_clients_by_session` so the
    // agent's WS handler can push relayed `TcpForwardAccept` etc.
    // back to this client.
    let (outbound_tx, outbound_rx) = mpsc::channel::<ServerMsg>(OUTBOUND_CAP);
    let pump = tokio::spawn(pump_server_messages(outbound_rx, socket_tx.clone()));
    // #4571 — the server's keepalive, beside the pump on the write side. The
    // read loop below ends the socket once the client has been silent for
    // `rx_deadline`.
    let (ping_every, rx_deadline) = keepalive(&state.settings.rc);
    let pinger = spawn_keepalive(socket_tx.clone(), ping_every);

    // Register this connection in the overlay registry (keyed by
    // tunnel_client_id) so the overlay broker can fan netmaps/deltas to
    // it as a node. Harmless if the client never joins the overlay;
    // removed on disconnect below.
    state
        .overlay_nodes_by_id
        .insert(tunnel_client_id, outbound_tx.clone());

    // Look up tunnel-client metadata once for audit-row enrichment
    // (client_version + client_os). Best-effort — audit rows still
    // get written if this fails, just with empty version/os.
    //
    // LIVE (#1829): the connect-time check already refused a tombstone; a
    // removal in the gap between it and here means there is nothing to
    // enrich with, the WARN below names the client, and the 60 s poll's
    // kick is on its way.
    let (client_version, client_os) = match state
        .tunnel_clients
        .find_live_in_tenant(tenant_id, tunnel_client_id)
        .await
    {
        Ok(c) => (c.client_version, c.os),
        Err(e) => {
            warn!(%tunnel_client_id, %e, "tunnel-client lookup failed; audit rows will be sparse");
            (
                String::new(),
                roomler_ai_remote_control::models::OsKind::Linux,
            )
        }
    };

    // Per-connection session state. Set on `TunnelOpen` and carried
    // by every audit row + outbound message.
    let mut session: Option<TunnelSession> = None;

    // Transports the client advertised in `rc:tunnel.hello`. Captured
    // here so `TunnelOpen` can intersect them with the requested
    // transport (Phase 1c). Empty until hello arrives → quic-v1 is
    // never negotiated, which is the safe default.
    let mut client_supported_transports: Vec<String> = Vec::new();

    // Bundle the originator identity + reply channel once. Every tunnel
    // handler is subject-agnostic (P3b-2) and takes this by ref, so the
    // very same open/forward/relay/audit code serves an agent driving the
    // tunnel-client role over its own WS (see
    // `relay_tunnel_client_msg_from_agent`). `client_version` is moved in
    // here; `client_os` + the ids are Copy, so the locals remain for the
    // logs / revocation task / overlay teardown below.
    let orig = Originator {
        principal: Principal::TunnelClient(tunnel_client_id),
        tenant_id,
        owner_user_id,
        client_version,
        client_os,
        outbound_tx: outbound_tx.clone(),
        dialed_tid,
        conn_established_ms,
        server_ended: roomler_core::ServerEnded::default(),
    };

    // Periodic revocation re-check task — same as T1 stub, but now
    // sends a typed `TunnelRevoked` `ServerMsg` instead of an
    // ad-hoc JSON frame. When it ends the connection it signals this
    // loop on `end_tx` first, and waits, at most `END_TOLD_WITHIN`, for
    // the loop to have told the session's exit.
    let (end_tx, mut end_rx) = mpsc::channel::<oneshot::Sender<()>>(1);
    let revocation_handle = spawn_revocation_check(
        state.clone(),
        socket_tx.clone(),
        outbound_tx.clone(),
        end_tx,
        orig.server_ended.clone(),
        tunnel_client_id,
        tenant_id,
        presence.clone(),
    );

    // Main read loop. #4571 — it ends a socket that has delivered no frame
    // for `rx_deadline` as it ends one whose read failed: a client that went
    // without closing (a laptop that slept, a NAT that dropped its mapping)
    // otherwise kept its seat and its presence record until a write failed.
    let silence = tokio::time::sleep(rx_deadline);
    tokio::pin!(silence);
    loop {
        let msg = tokio::select! {
            // The end signal first, which comes once; the deadline last, as a
            // frame that has arrived proves the client however late the loop
            // reads it.
            biased;
            Some(told) = end_rx.recv() => {
                if let Some((agent_id, msg)) = exit_end(&orig, session.as_ref()) {
                    info!(%tunnel_client_id, exit = %agent_id, "tunnel-client connection ends; the session's exit closes its connection");
                    let _ = state.fleet.rc_hub.send_to_agent(agent_id, msg);
                }
                let _ = told.send(());
                continue;
            }
            msg = socket_rx.next() => {
                // Every frame resets the deadline before it is matched: a
                // message, the client's own Ping, its Pong to ours.
                silence
                    .as_mut()
                    .reset(tokio::time::Instant::now() + rx_deadline);
                msg
            }
            () = &mut silence => {
                info!(
                    %tunnel_client_id, silent_s = rx_deadline.as_secs(),
                    "tunnel-client WS delivered no frame within its receive deadline; ending it as a lost link"
                );
                // A lost link: the read error's path below, and its teardown.
                Some(Err(axum::Error::new(std::io::Error::from(
                    std::io::ErrorKind::TimedOut,
                ))))
            }
        };
        let Some(msg) = msg else { break };
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => continue,
        };

        let parsed: ClientMsg = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                warn!(%tunnel_client_id, %e, "tunnel-client sent malformed JSON; dropping");
                send_error(
                    &outbound_tx,
                    None,
                    "bad_request",
                    &format!("malformed JSON: {e}"),
                )
                .await;
                continue;
            }
        };

        // Overlay `rc:overlay.*` variants are brokered separately (this
        // tunnel-client is an overlay node). Consumed messages return
        // None; everything else falls through to the tunnel match below.
        let Some(parsed) = crate::overlay::relay_overlay_msg_from_node(
            &state,
            crate::overlay::NodeIdentity::TunnelClient(tunnel_client_id),
            parsed,
        )
        .await
        else {
            continue;
        };

        match parsed {
            ClientMsg::TunnelHello {
                role: _,
                version,
                supported_transports,
            } => {
                debug!(%tunnel_client_id, %version, ?supported_transports, "rc:tunnel.hello");
                // Stash the advertised transports; `TunnelOpen`
                // intersects them with the requested transport to pick
                // the data plane (quic-v1 when both name it, else the
                // webrtc-dc-v1 default).
                client_supported_transports = supported_transports;
            }

            ClientMsg::TunnelOpen {
                agent_id,
                transport,
                open_nonce,
                derp_pubkey,
                net_grant,
            } => {
                // The dedicated CLI is single-open, so `open_nonce` is
                // normally None; the server echoes back whatever it sent.
                // A grant here is refused, as everywhere since FR-137 parked
                // FR-90 (S-B3).
                session = handle_tunnel_open(
                    &state,
                    &orig,
                    agent_id,
                    transport,
                    open_nonce,
                    derp_pubkey,
                    net_grant,
                    &client_supported_transports,
                )
                .await;
            }

            ClientMsg::TcpForwardRequest {
                session_id,
                flow_id,
                dst_host,
                dst_port,
            } => {
                handle_forward_request(
                    ProtocolKind::Tcp,
                    &state,
                    &orig,
                    session.as_ref(),
                    session_id,
                    flow_id,
                    &dst_host,
                    dst_port,
                )
                .await;
            }

            ClientMsg::UdpForwardRequest {
                session_id,
                flow_id,
                dst_host,
                dst_port,
            } => {
                handle_forward_request(
                    ProtocolKind::Udp,
                    &state,
                    &orig,
                    session.as_ref(),
                    session_id,
                    flow_id,
                    &dst_host,
                    dst_port,
                )
                .await;
            }

            ClientMsg::TunnelTerminate { session_id, reason } => {
                info!(%tunnel_client_id, %session_id, ?reason, "rc:tunnel.terminate");
                // Relay the teardown to the agent so it tears down
                // its peer state too. Best-effort — agent may be
                // offline.
                if let Some(s) = session.as_ref()
                    && s.tunnel_session_id == session_id
                {
                    let _ = state.fleet.rc_hub.send_to_agent(
                        s.agent_id,
                        orig.exit_terminate(s.tunnel_session_id, reason),
                    );
                }
                if let Some(s) = session.take() {
                    state.tunnel_clients_by_session.remove(&s.tunnel_session_id);
                    unindex_session_target(
                        &state.tunnel_sessions_by_target_agent,
                        s.agent_id,
                        s.tunnel_session_id,
                    );
                    audit_peer_close(&state, &s, &orig).await;
                }
            }

            ClientMsg::TcpHalfClose {
                session_id,
                flow_id,
                direction,
            } => {
                // Tunnel-client → server → agent: relay the half-close
                // for the agent's audit + write-half teardown. The
                // data-plane FIN itself rides the in-band SCTP
                // sentinel; this WS message is audit-only.
                relay_half_close_to_agent(
                    &state,
                    session.as_ref(),
                    tunnel_client_id,
                    session_id,
                    flow_id,
                    direction,
                )
                .await;
            }

            ClientMsg::TcpClosed {
                session_id,
                flow_id,
                reason,
                bytes_in,
                bytes_out,
            } => {
                // Relay flow-close to agent + append audit row. The byte
                // counts come from the endpoint because the payload never
                // passes through here.
                relay_tcp_closed_to_agent(
                    &state,
                    &orig,
                    session.as_ref(),
                    session_id,
                    flow_id,
                    reason,
                    (bytes_in, bytes_out),
                )
                .await;
            }

            ClientMsg::UdpClosed {
                session_id,
                flow_id,
                reason,
            } => {
                // Relay UDP flow-close to agent + append audit row.
                relay_udp_closed_to_agent(
                    &state,
                    &orig,
                    session.as_ref(),
                    session_id,
                    flow_id,
                    reason,
                )
                .await;
            }

            ClientMsg::TunnelSdpOffer { session_id, sdp } => {
                // Relay SDP offer to the agent so it can build its
                // answerer-side TunnelPeer. Server-side route is
                // session-id-gated; cross-tenant + agent-online
                // checks already happened on TunnelOpen.
                relay_sdp_offer_to_agent(&state, session.as_ref(), session_id, sdp).await;
            }

            ClientMsg::TunnelIce {
                session_id,
                candidate,
            } => {
                relay_ice_to_agent(&state, session.as_ref(), session_id, candidate).await;
            }

            ClientMsg::TunnelQuicCandidate { session_id, addrs } => {
                // QUIC-over-TURN (Phase 3c): the client's relay
                // address(es). Relay to the agent so it can install a
                // TURN permission for the client BEFORE the handshake.
                relay_quic_candidate_to_agent(&state, session.as_ref(), session_id, addrs).await;
            }

            ClientMsg::TunnelSdpAnswer { .. } => {
                // Clients only emit offers, agents only emit answers.
                // A client SDP answer means the wire is being abused.
                debug!(%tunnel_client_id, "client emitted TunnelSdpAnswer — ignoring");
            }

            ClientMsg::TcpForwardAccept { .. }
            | ClientMsg::TcpForwardReject { .. }
            | ClientMsg::UdpForwardAccept { .. }
            | ClientMsg::UdpForwardReject { .. } => {
                // Client → server: clients never originate these
                // (server-side ACL + agent are the deciders). Tests
                // exercise the wire; just log so we notice if a
                // misbehaving client emits one.
                debug!(%tunnel_client_id, ?parsed, "client emitted server-only tunnel msg — ignoring");
            }

            // Non-tunnel rc:* — explicitly ignored on this WS role.
            other => {
                debug!(%tunnel_client_id, ?other, "non-tunnel rc:* on tunnel-client WS — ignored");
            }
        }
    }

    // The socket has ended: no more keepalive.
    pinger.abort();
    revocation_handle.abort();
    // #4553 — waited out before the seat leaves: a refresh still in flight
    // would set the record again after the release deleted it
    // (`refresh_if_mine` writes a missing key), and the client would read
    // online for the record's whole TTL.
    let _ = revocation_handle.await;
    presence.close(presence_seat).await;

    // Overlay teardown: drop this node from the registry + mark it
    // offline so peers' netmaps lose it. Best-effort; no-op if the
    // client never joined the overlay.
    state.overlay_nodes_by_id.remove(&tunnel_client_id);
    crate::overlay::handle_overlay_leave(
        &state,
        crate::overlay::NodeIdentity::TunnelClient(tunnel_client_id),
    )
    .await;

    if let Some(s) = session {
        state.tunnel_clients_by_session.remove(&s.tunnel_session_id);
        unindex_session_target(
            &state.tunnel_sessions_by_target_agent,
            s.agent_id,
            s.tunnel_session_id,
        );
        // Best-effort: tell the agent the peer is gone so it tears
        // down its side.
        let _ = state.fleet.rc_hub.send_to_agent(
            s.agent_id,
            orig.exit_terminate(s.tunnel_session_id, CloseReason::ClientShutdown),
        );
        audit_peer_close(&state, &s, &orig).await;
    }
    // Drop our outbound_tx so the pump task can exit cleanly. Any
    // clones in tunnel_clients_by_session were just removed; any
    // clones the revocation task held are dropped when its handle
    // aborts. Defensive `pump.abort()` covers the edge case where
    // some other clone is still alive.
    drop(outbound_tx);
    pump.abort();
    info!(%tunnel_client_id, "tunnel-client WS disconnected");
}

/// Per-connection state created on `TunnelOpen` and consumed by every
/// subsequent flow event for audit correlation.
#[derive(Debug, Clone)]
pub struct TunnelSession {
    pub tunnel_session_id: ObjectId,
    agent_id: ObjectId,
    agent_tenant_id: ObjectId,
    #[allow(dead_code)] // T2.6 will plumb this into the agent-WS relay
    transport: String,
}

/// Identity + reply channel of whoever originated a tunnel-CLIENT session
/// on this server. Two principals drive the identical handlers:
///   * `Principal::TunnelClient` — a dedicated `roomler` WS
///     (`handle_tunnel_client_socket`).
///   * `Principal::Agent` — an enrolled agent driving the tunnel-client
///     role over its own agent WS (P3b-2,
///     `relay_tunnel_client_msg_from_agent`).
///
/// Bundling the per-originator values here lets ONE subject-agnostic set
/// of handlers serve both WS roles instead of duplicating the open /
/// forward / relay / audit logic per role.
#[derive(Clone)]
pub struct Originator {
    pub principal: Principal,
    /// The originator's own tenant. For an agent origin this is the
    /// agent's tenant — the value the cross-tenant gate compares against
    /// the TARGET agent's tenant.
    pub tenant_id: ObjectId,
    pub owner_user_id: ObjectId,
    pub client_version: String,
    pub client_os: OsKind,
    /// The originator's outbound `ServerMsg` channel. For a tunnel-client
    /// WS this is its per-connection `outbound_tx`; for an agent it is a
    /// clone of the agent's Hub-owned `registered_tx`, so a target's
    /// answers relayed via `tunnel_clients_by_session[session_id]` reach
    /// the agent's socket through its EXISTING pump — no target-side change.
    pub outbound_tx: mpsc::Sender<ServerMsg>,
    /// PR-1 rehome — the affinity key this conn DIALED with (`None` =
    /// key-less legacy dial that hashed on client IP) and when it
    /// established. Direction inputs for the cross-pod open-reject: only
    /// a correctly-keyed, provably-newer conn justifies nudging the
    /// target agent's WS.
    pub dialed_tid: Option<String>,
    pub conn_established_ms: i64,
    /// Set when the server ends this originator itself: the connection
    /// loop's end signal for a tunnel-client WS, the agent connection's
    /// [`roomler_core::AgentCtx::server_ended`] for an agent. Every
    /// terminate an exit gets for this originator's sessions from then on
    /// asks it to close the session's connection ([`Self::exit_terminate`]).
    pub server_ended: roomler_core::ServerEnded,
}

impl Originator {
    /// The `rc:tunnel.terminate` an exit gets for one of this originator's
    /// sessions. It asks the exit to close the session's connection once the
    /// server ended this originator itself, and not before.
    pub fn exit_terminate(&self, session_id: ObjectId, reason: CloseReason) -> ServerMsg {
        ServerMsg::TunnelTerminate {
            session_id,
            reason,
            close_connection: self.server_ended.get(),
        }
    }

    /// The originating tunnel-client row id, or `None` for an agent origin.
    fn tunnel_client_id(&self) -> Option<ObjectId> {
        match self.principal {
            Principal::TunnelClient(id) => Some(id),
            Principal::Agent(_) => None,
        }
    }
    /// The originating agent id, or `None` for a dedicated-client origin.
    fn origin_agent_id(&self) -> Option<ObjectId> {
        match self.principal {
            Principal::Agent(id) => Some(id),
            Principal::TunnelClient(_) => None,
        }
    }
    /// Stable id for log lines, regardless of principal kind.
    fn log_id(&self) -> ObjectId {
        match self.principal {
            Principal::TunnelClient(id) | Principal::Agent(id) => id,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_tunnel_open(
    state: &NetworkState,
    orig: &Originator,
    agent_id: ObjectId,
    transport: String,
    open_nonce: Option<String>,
    // R4 - the client's DERP identity from TunnelOpen (quic-derp-v1 only).
    derp_pubkey: Option<String>,
    // FR-137 S-B3 - an agent session's grant (FR-90, parked), which an older
    // agent may still send. Refused, never verified.
    net_grant: Option<String>,
    client_supported: &[String],
) -> Option<TunnelSession> {
    // 1. Fetch the agent (any tenant — we need the row to enforce
    // the cross-tenant gate ourselves). `find_in_tenant` is wrong
    // here because it scopes by tenant — we need the agent's actual
    // tenant_id to compare. Use a direct lookup via the base DAO.
    let agent = match state.fleet.agents.base.find_by_id(agent_id).await {
        Ok(a) => a,
        Err(_) => {
            send_msg(
                &orig.outbound_tx,
                ServerMsg::Error {
                    session_id: None,
                    code: "agent_not_found".into(),
                    message: format!("device {agent_id} does not exist"),
                    open_nonce: open_nonce.clone(),
                },
            )
            .await;
            return None;
        }
    };

    // 2. Cross-tenant gate (Sev0 — see plan §"Multi-tenancy gotcha").
    // The originator's tenant is the client tenant; for an agent origin
    // it is the agent's own tenant.
    if agent.tenant_id != orig.tenant_id {
        warn!(
            origin = %orig.log_id(), %agent_id, client_tenant_id = %orig.tenant_id,
            agent_tenant_id = %agent.tenant_id,
            "tunnel originator tried to open a peer to a cross-tenant agent"
        );
        send_msg(
            &orig.outbound_tx,
            ServerMsg::Error {
                session_id: None,
                code: "cross_tenant".into(),
                message: "device belongs to a different tenant".into(),
                open_nonce: open_nonce.clone(),
            },
        )
        .await;
        return None;
    }

    // 3. Refuse if the TARGET agent is soft-deleted or quarantined —
    // early signal beats waiting for the relay step to fail.
    if agent.deleted_at.is_some() || matches!(agent.status, AgentStatus::Quarantined) {
        send_msg(
            &orig.outbound_tx,
            ServerMsg::Error {
                session_id: None,
                code: "agent_unavailable".into(),
                message: "device is quarantined or deleted".into(),
                open_nonce: open_nonce.clone(),
            },
        )
        .await;
        return None;
    }

    // 3a½ (C-3). REHOME gate: the target agent isn't in THIS pod's hub but
    // a FOREIGN pod holds a fresh presence record — a cross-pod split, not
    // an offline agent. Without this, the open would "succeed" and every
    // forward would then bounce off `device unreachable` FOREVER (the client
    // keeps its parked WS and never re-dials). Reject the open with the
    // rehome code — the CLI driver bails on open-errors, its reconnect
    // loop dials a FRESH WS, and the LB re-hashes it onto the owner pod —
    // and nudge the owner to cycle the agent if idle so both ends
    // converge. Plain hub-miss with no foreign record keeps today's
    // behaviour (the P7 terminate machinery owns real agent flaps).
    if !state.fleet.rc_hub.is_agent_online(agent_id)
        && let Some(redis) = &state.redis_pubsub
        && let Ok(Ok(Some(owner))) = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            redis.agent_presence_foreign(&agent_id.to_hex()),
        )
        .await
    {
        let record = roomler_core::cluster::directory::OwnerRecord::parse(&owner);
        let owner_pod = record
            .as_ref()
            .map(|r| r.pod_id.clone())
            .unwrap_or_default();
        let agent_since_ms = record.map(|r| r.since_ms).unwrap_or(0);
        warn!(
            origin = %orig.log_id(), %agent_id, %owner_pod,
            "tunnel open rejected: agent homed on another pod (rehome)"
        );
        // PR-1 direction rule (twin of the rc path): only a correctly-
        // keyed conn that is provably NEWER than the target agent's
        // registration justifies cycling that agent's WS; a key-less /
        // wrong-keyed / older-or-ambiguous originator is itself the
        // mis-placed party and its own fresh redial converges it.
        // `agent` (the target's row) is already fetched above.
        match roomler_ai_mod_fleet::nudge::rehome_direction(
            orig.dialed_tid.as_deref(),
            &agent.tenant_id.to_hex(),
            orig.conn_established_ms,
            agent_since_ms,
            state.settings.rc.rehome_direction_guard_ms,
        ) {
            roomler_ai_mod_fleet::nudge::RehomeDirection::ControllerMove { reason } => {
                roomler_core::cluster::metrics::bump(
                    &roomler_core::cluster::metrics::RC_REHOME_CONTROLLER_TOTAL,
                );
                info!(
                    origin = %orig.log_id(), %agent_id, %owner_pod, reason,
                    "cross-pod tunnel miss: originator re-dials (nudge suppressed)"
                );
            }
            roomler_ai_mod_fleet::nudge::RehomeDirection::NudgeAgent => {
                roomler_ai_mod_fleet::nudge::spawn_agent_nudge(
                    &state.fleet,
                    owner_pod.clone(),
                    agent_id.to_hex(),
                );
            }
        }
        roomler_core::cluster::metrics::bump(&roomler_core::cluster::metrics::TUNNEL_REHOME_TOTAL);
        // Pod identity stays in server logs; the wire carries only the
        // actionable fact + the open_nonce so the CLI fails the exact
        // pending flow and its reconnect loop dials fresh.
        send_msg(
            &orig.outbound_tx,
            ServerMsg::Error {
                session_id: None,
                code: "agent_on_other_pod".into(),
                message: "device is homed on another pod; re-dial and retry".into(),
                open_nonce: open_nonce.clone(),
            },
        )
        .await;
        return None;
    }

    // 3b. Defence-in-depth for an AGENT origin (P3b-2 Risk 7): refuse if
    // the ORIGINATING agent's row is soft-deleted / quarantined. Its WS
    // auth already gated it at connect and the Hub closes a deleted
    // agent's socket, but re-checking at the origination entry point stops
    // a mid-session quarantine from spawning a fresh tunnel.
    if let Principal::Agent(origin_id) = orig.principal {
        let origin_ok = matches!(
            state.fleet.agents.base.find_by_id(origin_id).await,
            Ok(a) if a.deleted_at.is_none() && !matches!(a.status, AgentStatus::Quarantined)
        );
        if !origin_ok {
            send_msg(
                &orig.outbound_tx,
                ServerMsg::Error {
                    session_id: None,
                    code: "origin_unavailable".into(),
                    message: "originating device is quarantined or deleted".into(),
                    open_nonce: open_nonce.clone(),
                },
            )
            .await;
            return None;
        }
    }

    // 3c. FR-137 S-B3 — an agent SESSION's grant (FR-90, parked) refuses the
    // open whenever one is presented. The wire keeps the field, parse-only,
    // while a 0.4.199 agent may still send one, and the open must never fall
    // back to the device's own principal, which would hand the session
    // whatever its device may reach.
    if net_grant.is_some() {
        let why = "an agent session's grant, and FR-90 is parked";
        warn!(origin = %orig.log_id(), %agent_id, why, "tunnel open refused");
        send_msg(
            &orig.outbound_tx,
            ServerMsg::Error {
                session_id: None,
                code: NET_GRANT_REFUSED.into(),
                message: why.into(),
                open_nonce: open_nonce.clone(),
            },
        )
        .await;
        return None;
    }

    // Transport negotiation (Phase 1c + Phase 4 agent-version gate).
    // Negotiate quic-v1 only when the client requested it, advertised it
    // in rc:tunnel.hello, AND the target agent's reported version
    // actually speaks the QUIC tunnel setup. Otherwise fall back to the
    // proven webrtc-dc-v1.
    //
    // The agent-version gate is what makes `--transport auto` safe as
    // the client default: a pre-rc.104 agent silently ignores the
    // TunnelQuicSetup below and never replies, so without this gate the
    // client would burn its full QUIC_READY_TIMEOUT (30 s) before
    // falling back. `agent_version` is refreshed on every agent WS
    // connect (update_hello), so it's current for any *online* agent —
    // and only an online agent can be tunneled to (send_to_agent fails
    // otherwise). #3565: "" while the daemon has not reported one, which
    // both gates read as too old.
    let agent_version = agent.reported_version().unwrap_or("");
    let agent_speaks_quic = tunnel_core::transport::agent_supports_quic(agent_version);
    // R4 - quic-derp-v1: negotiated only when the client REQUESTED it,
    // advertised it, sent its DERP identity, and the agent's version can
    // serve it (a pre-R4 agent would ignore the setup's transport field,
    // stand up a TURN relay, and the client would burn its 30s ready-wait).
    // A derp request that cannot be honored degrades to plain quic-v1 when
    // possible (better than webrtc through a captured path), else webrtc.
    let agent_speaks_derp = tunnel_core::transport::agent_supports_derp_tunnel(agent_version);
    let derp_requested = transport == tunnel_core::transport::TRANSPORT_QUIC_DERP_V1;
    let negotiated_transport = if derp_requested {
        if client_supported
            .iter()
            .any(|t| t == tunnel_core::transport::TRANSPORT_QUIC_DERP_V1)
            && agent_speaks_derp
            && derp_pubkey.is_some()
        {
            tunnel_core::transport::TRANSPORT_QUIC_DERP_V1.to_string()
        } else if client_supported.iter().any(|t| t == TRANSPORT_QUIC_V1) && agent_speaks_quic {
            debug!(
                %agent_id, %agent_version, agent_speaks_derp,
                had_pubkey = derp_pubkey.is_some(),
                "client requested quic-derp-v1 -> negotiating quic-v1 (derp leg unavailable)"
            );
            TRANSPORT_QUIC_V1.to_string()
        } else {
            TRANSPORT_WEBRTC_DC_V1.to_string()
        }
    } else if transport == TRANSPORT_QUIC_V1 {
        if client_supported.iter().any(|t| t == TRANSPORT_QUIC_V1) && agent_speaks_quic {
            TRANSPORT_QUIC_V1.to_string()
        } else {
            debug!(
                %agent_id, %agent_version, agent_speaks_quic,
                "client requested quic-v1 → negotiating webrtc-dc-v1 (agent too old, or client didn't advertise quic)"
            );
            TRANSPORT_WEBRTC_DC_V1.to_string()
        }
    } else {
        transport.clone()
    };
    // Mint a per-session bearer token for quic-v1; the agent validates
    // the direct P2P link by string-equality (see
    // tunnel_core::transport::quic::server_authenticate). `None` for
    // webrtc-dc-v1 keeps those sessions byte-identical on the wire.
    let quic_auth_token = if negotiated_transport == TRANSPORT_QUIC_V1
        || negotiated_transport == tunnel_core::transport::TRANSPORT_QUIC_DERP_V1
    {
        Some(mint_quic_token())
    } else {
        None
    };

    // 4. Create the session id + register the outbound channel so the
    // agent WS handler can relay TcpForwardAccept/Reject/HalfClose/Closed
    // (+ TunnelQuicReady) back to the originator. For an agent origin this
    // channel IS the agent's own socket, so the existing target->client
    // relay path serves it unchanged. The caller stores the returned
    // session (in its `Option` or its per-agent session map).
    let tunnel_session_id = ObjectId::new();
    let new_session = TunnelSession {
        tunnel_session_id,
        agent_id,
        agent_tenant_id: agent.tenant_id,
        transport: negotiated_transport.clone(),
    };
    state
        .tunnel_clients_by_session
        .insert(tunnel_session_id, orig.outbound_tx.clone());
    // C-3 — ownership record: which pod owns this session's WS legs
    // (observability + the C-6 metrics; refreshed by the directory
    // heartbeat while the map entry above lives, TTL-reaped after).
    if let Some(dir) = &state.cluster_directory {
        let token = dir.owner_token(&tunnel_session_id.to_hex());
        if let Err(e) = dir
            .claim_lww(
                &roomler_core::cluster::directory::tunnel_key(&tunnel_session_id.to_hex()),
                &token,
            )
            .await
        {
            // Kept anyway (#3840): the heartbeat re-sets an absent record.
            debug!(%tunnel_session_id, %e, "tunnel ownership claim failed");
        }
        state
            .tunnel_presence_tokens
            .insert(tunnel_session_id, token);
    }
    // P7 flap resilience: index the session by its TARGET agent so
    // `terminate_sessions_targeting_agent` can kill it the moment that
    // agent's WS drops (the agent's tunnel peers die with its socket).
    state
        .tunnel_sessions_by_target_agent
        .entry(agent_id)
        .or_default()
        .insert(tunnel_session_id);

    // 5. Audit the open. RelayMode is "Direct" until ICE finishes —
    // T2.7 updates this after candidate selection.
    let _ = state
        .tunnel_audit
        .append(&TunnelAuditEvent {
            id: None,
            tenant_id: orig.tenant_id,
            tunnel_session_id,
            tunnel_client_id: orig.tunnel_client_id(),
            origin_agent_id: orig.origin_agent_id(),
            agent_id,
            user_id: orig.owner_user_id,
            at: DateTime::now(),
            kind: TunnelAuditKind::PeerOpen,
            flow_id: None,
            dst_host: None,
            dst_port: None,
            bytes_in: 0,
            bytes_out: 0,
            message_count: 0,
            duration_ms: None,
            relay: RelayMode::Direct,
            client_src_ip: None, // T2.6 — extract X-Forwarded-For at upgrade
            agent_src_port: None,
            client_version: orig.client_version.clone(),
            client_os: orig.client_os,
            reason: None,
        })
        .await;

    // QUIC-over-TURN (Phase 3c): mint short-lived coturn creds for THIS
    // session and hand the SAME set to both peers so each can allocate
    // its own TURN relay. Empty when no TURN `shared_secret` is
    // configured (dev / direct-only) — the QUIC path then stays on
    // host/srflx candidates. `user_id` for the HMAC is the session id so
    // both peers derive identical, session-scoped creds.
    // FR-115 P4b — charged to the tunnel's org, so coturn's statistics name it.
    let quic_ice_servers = roomler_ai_remote_control::turn_creds::ice_servers_for(
        &roomler_ai_remote_control::turn_creds::charged(
            &tunnel_session_id.to_hex(),
            &orig.tenant_id.to_hex(),
        ),
        // Region-keyed map; `None` = default region until the agent's
        // relay_home is threaded through (probe reports, PR3).
        state.turn_map.cfg_for(None),
    );

    // For quic-v1, tell the agent to stand up its endpoint + authorize
    // this token NOW. The client is already registered in
    // `tunnel_clients_by_session` (step 4), so the agent's
    // `TunnelQuicReady` can race back and be relayed to the client
    // while it's still processing the `TunnelOpened` below.
    if let Some(token) = quic_auth_token.clone()
        && let Err(e) = state.fleet.rc_hub.send_to_agent(
            agent_id,
            ServerMsg::TunnelQuicSetup {
                session_id: tunnel_session_id,
                quic_auth_token: token,
                ice_servers: quic_ice_servers.clone(),
                transport: (negotiated_transport == tunnel_core::transport::TRANSPORT_QUIC_DERP_V1)
                    .then(|| negotiated_transport.clone()),
                client_derp_pubkey: if negotiated_transport
                    == tunnel_core::transport::TRANSPORT_QUIC_DERP_V1
                {
                    derp_pubkey.clone()
                } else {
                    None
                },
            },
        )
    {
        warn!(%agent_id, %e, "TunnelQuicSetup relay failed; client will fall back to webrtc-dc-v1");
    }

    // 6. Reply with TunnelOpened carrying the NEGOTIATED transport +
    // (for quic-v1) the bearer token. ICE candidates + actual SDP come
    // in T2.7 for the webrtc path; quic carries its own handshake. The
    // SCTP rwnd value mirrors the vendored webrtc patch's target so the
    // CLI's `diagnose` subcommand can verify the patch took effect.
    send_msg(
        &orig.outbound_tx,
        ServerMsg::TunnelOpened {
            session_id: tunnel_session_id,
            transport: negotiated_transport,
            dc_pool_size: 8,
            sctp_rwnd_bytes: 8 * 1024 * 1024,
            ice_servers: quic_ice_servers,
            quic_auth_token,
            open_nonce,
        },
    )
    .await;

    Some(new_session)
}

/// Mint an opaque per-session QUIC bearer token. The server hands the
/// SAME value to the client (`TunnelOpened.quic_auth_token`) and the
/// agent (`TunnelQuicSetup.quic_auth_token`); the agent authenticates
/// the direct P2P QUIC connection by string-equality. It needs no
/// shared secret or verifiable signature because both endpoints receive
/// it over their already-authenticated WS connections — it only needs
/// to be unguessable. Two v4 UUIDs = 244 bits of randomness.
fn mint_quic_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Build the transport-appropriate reject ServerMsg. TCP and UDP
/// forwards share the identical gate logic; only the wire discriminator
/// differs.
fn forward_reject_msg(
    proto: ProtocolKind,
    session_id: ObjectId,
    flow_id: u32,
    kind: RejectKind,
    reason: String,
) -> ServerMsg {
    match proto {
        ProtocolKind::Udp => ServerMsg::UdpForwardReject {
            session_id,
            flow_id,
            kind,
            reason,
        },
        _ => ServerMsg::TcpForwardReject {
            session_id,
            flow_id,
            kind,
            reason,
        },
    }
}

/// Build the transport-appropriate forward ServerMsg (server → agent).
fn forward_forward_msg(
    proto: ProtocolKind,
    session_id: ObjectId,
    flow_id: u32,
    dst_host: String,
    dst_port: u16,
    owner_user_id: ObjectId,
) -> ServerMsg {
    match proto {
        ProtocolKind::Udp => ServerMsg::UdpForwardForward {
            session_id,
            flow_id,
            dst_host,
            dst_port,
            owner_user_id,
        },
        _ => ServerMsg::TcpForwardForward {
            session_id,
            flow_id,
            dst_host,
            dst_port,
            owner_user_id,
        },
    }
}

/// The role ids a forward is matched with (#3635): what `lookup` reads, which
/// in the gate is the roles the originator's owner holds in the org whose
/// `policies` decide the forward, read on every forward request so a role
/// given or taken back counts from the next forward on.
///
/// - No live policy names a role: empty, and `lookup` is never called. A
///   role list can change no decision then, and the common case should not
///   pay a read for the rare one.
/// - `lookup` fails: empty, with a WARN naming the origin. Role subjects then
///   match nobody and every other subject kind evaluates as before; an
///   unreadable role list never turns into an allow.
async fn forward_role_ids<F, Fut, E>(
    policies: &[TunnelPolicy],
    lookup: F,
    origin: ObjectId,
) -> Vec<ObjectId>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<ObjectId>, E>>,
    E: std::fmt::Display,
{
    let names_a_role = policies.iter().any(|p| {
        p.deleted_at.is_none()
            && p.subjects
                .iter()
                .any(|s| matches!(s, PolicySubject::RoleId { .. }))
    });
    if !names_a_role {
        return Vec::new();
    }
    match lookup().await {
        Ok(role_ids) => role_ids,
        Err(e) => {
            warn!(%origin, %e, "tunnel forward: roles unreadable; role subjects will not match this forward");
            Vec::new()
        }
    }
}

/// Server-side forward gate + relay, shared by TCP CONNECT
/// (`proto = Tcp`) and UDP ASSOCIATE (`proto = Udp`) forwards. The ACL
/// eval, cross-tenant gate, agent-liveness re-check, and audit trail are
/// identical; `proto` selects the wire discriminator + the policy
/// protocol axis.
#[allow(clippy::too_many_arguments)]
async fn handle_forward_request(
    proto: ProtocolKind,
    state: &NetworkState,
    orig: &Originator,
    session: Option<&TunnelSession>,
    request_session_id: ObjectId,
    flow_id: u32,
    dst_host: &str,
    dst_port: u16,
) {
    let Some(s) = session else {
        // No prior TunnelOpen — client is using the wire wrong.
        send_msg(
            &orig.outbound_tx,
            forward_reject_msg(
                proto,
                request_session_id,
                flow_id,
                RejectKind::AgentError,
                REJECT_REASON_NO_SESSION.into(),
            ),
        )
        .await;
        return;
    };
    if s.tunnel_session_id != request_session_id {
        send_msg(
            &orig.outbound_tx,
            forward_reject_msg(
                proto,
                request_session_id,
                flow_id,
                RejectKind::AgentError,
                REJECT_REASON_SESSION_MISMATCH.into(),
            ),
        )
        .await;
        return;
    }

    // Re-fetch the agent row each time so a quarantine that lands
    // mid-session bites the next forward.
    let agent = match state.fleet.agents.base.find_by_id(s.agent_id).await {
        Ok(a) => a,
        Err(_) => {
            audit_tcp_reject(
                state,
                s,
                orig,
                flow_id,
                dst_host,
                dst_port,
                RejectKind::AgentError,
                "device row vanished",
            )
            .await;
            send_msg(
                &orig.outbound_tx,
                forward_reject_msg(
                    proto,
                    request_session_id,
                    flow_id,
                    RejectKind::AgentError,
                    "device row vanished".into(),
                ),
            )
            .await;
            return;
        }
    };

    // The org whose policies decide this forward: the target device's. A
    // forward between two orgs is refused below (`CrossTenant`) whatever its
    // policies say.
    let policy_tenant = s.agent_tenant_id;

    // Active policies for the agent's tenant. Server-side ACL is
    // the auth boundary; the agent runs its own minimal allowlist
    // as defence-in-depth (T2.6).
    //
    // ⚠️ No permission bit is read here, by design: the policy IS the grant,
    // and it exists to let people who hold no management bit reach what it
    // names. What keeps that sound is who may WRITE a policy: `MANAGE_AGENTS`
    // (`routes::tunnel::tunnel_policy_manager`).
    let policies = match state
        .tunnel_policies
        .list_active_for_tenant(policy_tenant)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            warn!(origin = %orig.log_id(), %e, "policy fetch failed; defaulting to deny");
            Vec::new()
        }
    };

    // The roles a `RoleId` subject is matched against: those the originator's
    // owner holds in `policy_tenant`, read now, on this forward (#3635).
    let tenants = &state.tenants;
    let owner = orig.owner_user_id;
    let role_ids = forward_role_ids(
        &policies,
        move || tenants.member_live_role_ids(policy_tenant, owner),
        orig.log_id(),
    )
    .await;

    let subject = ResolvedSubject {
        // `UserId` subjects match the originator's owner: the person who
        // enrolled the tunnel client, or the device's owner, read when the
        // device's socket connected.
        user_id: orig.owner_user_id,
        role_ids,
        // Subject-agnostic (P3b-2): a tunnel-client WS carries
        // `Principal::TunnelClient`, an agent driving the client role over
        // its own WS carries `Principal::Agent`. The ACL evaluator
        // discriminates the id-specific subjects on the principal kind.
        principal: orig.principal,
    };

    let result = check_forward_request(
        orig.tenant_id,
        &agent,
        &policies,
        &subject,
        dst_host,
        dst_port,
        proto,
    );

    match result {
        GateResult::Allow { policy_id, .. } => {
            debug!(origin = %orig.log_id(), %flow_id, %dst_host, %dst_port, ?proto, %policy_id, "forward allowed by policy; relaying to agent");
            // T2.10c: relay to the agent's WS. The agent dials dst,
            // then replies with `ClientMsg::TcpForwardAccept` /
            // `UdpForwardAccept` (or Reject) which the agent WS handler
            // routes back to us via `tunnel_clients_by_session`.
            let relay = state.fleet.rc_hub.send_to_agent(
                s.agent_id,
                forward_forward_msg(
                    proto,
                    request_session_id,
                    flow_id,
                    dst_host.to_string(),
                    dst_port,
                    // Under a grant, the session's owner (P3-7-2a).
                    orig.owner_user_id,
                ),
            );
            match relay {
                Ok(()) => {
                    // Server side accepted-relayed; the actual
                    // accept (or agent-side reject) lands later
                    // via the agent's WS.
                    audit_tcp_accept(state, s, orig, flow_id, dst_host, dst_port).await;
                }
                Err(e) => {
                    // Agent not online or its channel is wedged.
                    warn!(origin = %orig.log_id(), %flow_id, agent = %s.agent_id, %e, "agent relay failed");
                    roomler_ai_mod_fleet::nudge::note_agent_offline_evidence(
                        state,
                        s.agent_id.to_hex(),
                        "tunnel_forward",
                    );
                    audit_tcp_reject(
                        state,
                        s,
                        orig,
                        flow_id,
                        dst_host,
                        dst_port,
                        RejectKind::AgentError,
                        &format!("device unreachable: {e}"),
                    )
                    .await;
                    send_msg(
                        &orig.outbound_tx,
                        forward_reject_msg(
                            proto,
                            request_session_id,
                            flow_id,
                            RejectKind::AgentError,
                            format!("device unreachable: {e}"),
                        ),
                    )
                    .await;
                }
            }
        }
        GateResult::Reject { kind, reason } => {
            info!(origin = %orig.log_id(), %flow_id, %dst_host, %dst_port, ?proto, ?kind, %reason, "forward rejected");
            audit_tcp_reject(state, s, orig, flow_id, dst_host, dst_port, kind, &reason).await;
            send_msg(
                &orig.outbound_tx,
                forward_reject_msg(proto, request_session_id, flow_id, kind, reason),
            )
            .await;
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Audit helpers — every interesting decision appends one row.
// ─────────────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn audit_tcp_accept(
    state: &NetworkState,
    session: &TunnelSession,
    orig: &Originator,
    flow_id: u32,
    dst_host: &str,
    dst_port: u16,
) {
    let _ = state
        .tunnel_audit
        .append(&TunnelAuditEvent {
            id: None,
            tenant_id: session.agent_tenant_id,
            tunnel_session_id: session.tunnel_session_id,
            tunnel_client_id: orig.tunnel_client_id(),
            origin_agent_id: orig.origin_agent_id(),
            agent_id: session.agent_id,
            user_id: orig.owner_user_id,
            at: DateTime::now(),
            kind: TunnelAuditKind::TcpAccept,
            flow_id: Some(flow_id),
            dst_host: Some(dst_host.to_string()),
            dst_port: Some(dst_port),
            bytes_in: 0,
            bytes_out: 0,
            message_count: 0,
            duration_ms: None,
            relay: RelayMode::Direct,
            client_src_ip: None,
            agent_src_port: None,
            client_version: orig.client_version.clone(),
            client_os: orig.client_os,
            reason: None,
        })
        .await;
}

#[allow(clippy::too_many_arguments)]
async fn audit_tcp_reject(
    state: &NetworkState,
    session: &TunnelSession,
    orig: &Originator,
    flow_id: u32,
    dst_host: &str,
    dst_port: u16,
    kind: RejectKind,
    reason: &str,
) {
    let _ = state
        .tunnel_audit
        .append(&TunnelAuditEvent {
            id: None,
            tenant_id: session.agent_tenant_id,
            tunnel_session_id: session.tunnel_session_id,
            tunnel_client_id: orig.tunnel_client_id(),
            origin_agent_id: orig.origin_agent_id(),
            agent_id: session.agent_id,
            user_id: orig.owner_user_id,
            at: DateTime::now(),
            kind: TunnelAuditKind::TcpReject,
            flow_id: Some(flow_id),
            dst_host: Some(dst_host.to_string()),
            dst_port: Some(dst_port),
            bytes_in: 0,
            bytes_out: 0,
            message_count: 0,
            duration_ms: None,
            relay: RelayMode::Direct,
            client_src_ip: None,
            agent_src_port: None,
            client_version: orig.client_version.clone(),
            client_os: orig.client_os,
            reason: Some(format!("{kind:?}: {reason}")),
        })
        .await;
}

async fn audit_peer_close(state: &NetworkState, session: &TunnelSession, orig: &Originator) {
    let _ = state
        .tunnel_audit
        .append(&TunnelAuditEvent {
            id: None,
            tenant_id: session.agent_tenant_id,
            tunnel_session_id: session.tunnel_session_id,
            tunnel_client_id: orig.tunnel_client_id(),
            origin_agent_id: orig.origin_agent_id(),
            agent_id: session.agent_id,
            user_id: orig.owner_user_id,
            at: DateTime::now(),
            kind: TunnelAuditKind::PeerClose,
            flow_id: None,
            dst_host: None,
            dst_port: None,
            bytes_in: 0,
            bytes_out: 0,
            message_count: 0,
            duration_ms: None,
            relay: RelayMode::Direct,
            client_src_ip: None,
            agent_src_port: None,
            client_version: orig.client_version.clone(),
            client_os: orig.client_os,
            reason: None,
        })
        .await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Wire helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Push `msg` onto the per-connection outbound channel. The pump task
/// serialises + writes to the socket. A closed channel means the
/// socket has gone away or the pump exited — log + drop.
async fn send_msg(outbound: &mpsc::Sender<ServerMsg>, msg: ServerMsg) {
    if let Err(e) = outbound.send(msg).await {
        debug!(%e, "outbound channel closed; dropping ServerMsg");
    }
}

async fn send_error(
    outbound: &mpsc::Sender<ServerMsg>,
    session_id: Option<ObjectId>,
    code: &str,
    message: &str,
) {
    send_msg(
        outbound,
        ServerMsg::Error {
            session_id,
            code: code.into(),
            message: message.into(),
            open_nonce: None,
        },
    )
    .await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Client → agent relays for `TcpHalfClose` / `TcpClosed`
// ─────────────────────────────────────────────────────────────────────────────

/// Relay a `ClientMsg::TcpHalfClose` from the tunnel-client to the
/// connected agent. Audit-only on the data plane (the in-band SCTP
/// sentinel does the actual mailbox close on the peer); this message
/// gives the agent's audit path a half-close event to record.
async fn relay_half_close_to_agent(
    state: &NetworkState,
    session: Option<&TunnelSession>,
    tunnel_client_id: ObjectId,
    request_session_id: ObjectId,
    flow_id: u32,
    direction: roomler_ai_remote_control::signaling::Direction,
) {
    let Some(s) = session else {
        debug!(%tunnel_client_id, %flow_id, "half-close on dead session — ignoring");
        return;
    };
    if s.tunnel_session_id != request_session_id {
        debug!(%tunnel_client_id, %flow_id, "half-close with mismatched session_id — ignoring");
        return;
    }
    if let Err(e) = state.fleet.rc_hub.send_to_agent(
        s.agent_id,
        ServerMsg::TcpHalfClose {
            session_id: request_session_id,
            flow_id,
            direction,
        },
    ) {
        debug!(%tunnel_client_id, %flow_id, %e, "half-close relay to agent failed");
    }
}

/// Relay a `ClientMsg::TcpClosed` from the tunnel-client to the
/// agent and append an audit row. The flow is fully closed at this
/// point; `tunnel_audit` records the close reason so admins can
/// reconstruct the lifecycle.
async fn relay_tcp_closed_to_agent(
    state: &NetworkState,
    orig: &Originator,
    session: Option<&TunnelSession>,
    request_session_id: ObjectId,
    flow_id: u32,
    reason: CloseReason,
    // `(bytes_in, bytes_out)` as the reporting endpoint counted them.
    bytes: (u64, u64),
) {
    let Some(s) = session else {
        return;
    };
    if s.tunnel_session_id != request_session_id {
        return;
    }
    if let Err(e) = state.fleet.rc_hub.send_to_agent(
        s.agent_id,
        ServerMsg::TcpClosed {
            session_id: request_session_id,
            flow_id,
            reason,
        },
    ) {
        debug!(origin = %orig.log_id(), %flow_id, %e, "tcp-closed relay to agent failed");
    }
    audit_tcp_close(state, s, orig, flow_id, reason, bytes).await;
}

/// UDP analogue of [`relay_tcp_closed_to_agent`]: relay a UDP-flow close
/// to the agent + append an audit row. Reuses [`audit_tcp_close`] — the
/// close accounting is L4-agnostic (flow_id + reason).
async fn relay_udp_closed_to_agent(
    state: &NetworkState,
    orig: &Originator,
    session: Option<&TunnelSession>,
    request_session_id: ObjectId,
    flow_id: u32,
    reason: CloseReason,
) {
    let Some(s) = session else {
        return;
    };
    if s.tunnel_session_id != request_session_id {
        return;
    }
    if let Err(e) = state.fleet.rc_hub.send_to_agent(
        s.agent_id,
        ServerMsg::UdpClosed {
            session_id: request_session_id,
            flow_id,
            reason,
        },
    ) {
        debug!(origin = %orig.log_id(), %flow_id, %e, "udp-closed relay to agent failed");
    }
    // UDP flows carry no byte counts: SOCKS5 UDP-ASSOCIATE datagrams run
    // on per-flow `FlowStats` with no session aggregate, and `UdpClosed`
    // has no counters to forward. Zero here means "not counted", same as
    // a pre-wave-3 client — not "no traffic".
    audit_tcp_close(state, s, orig, flow_id, reason, (0, 0)).await;
}

/// Relay a tunnel-client SDP offer to the agent. Cheap session_id
/// validation only — the heavy gates (cross-tenant, agent online)
/// already fired on TunnelOpen, so reaching here means the session
/// is sound.
async fn relay_sdp_offer_to_agent(
    state: &NetworkState,
    session: Option<&TunnelSession>,
    request_session_id: ObjectId,
    sdp: String,
) {
    let Some(s) = session else {
        debug!(%request_session_id, "SDP offer on dead session — ignoring");
        return;
    };
    if s.tunnel_session_id != request_session_id {
        debug!(%request_session_id, "SDP offer session_id mismatch — ignoring");
        return;
    }
    if let Err(e) = state.fleet.rc_hub.send_to_agent(
        s.agent_id,
        ServerMsg::TunnelSdpOffer {
            session_id: request_session_id,
            sdp,
        },
    ) {
        warn!(%request_session_id, %e, "SDP offer relay to agent failed");
    }
}

/// Relay a tunnel ICE candidate to the agent. Symmetric to
/// [`relay_sdp_offer_to_agent`].
async fn relay_ice_to_agent(
    state: &NetworkState,
    session: Option<&TunnelSession>,
    request_session_id: ObjectId,
    candidate: serde_json::Value,
) {
    let Some(s) = session else {
        return;
    };
    if s.tunnel_session_id != request_session_id {
        return;
    }
    if let Err(e) = state.fleet.rc_hub.send_to_agent(
        s.agent_id,
        ServerMsg::TunnelIce {
            session_id: request_session_id,
            candidate,
        },
    ) {
        debug!(%request_session_id, %e, "tunnel ICE relay to agent failed");
        roomler_ai_mod_fleet::nudge::note_agent_offline_evidence(
            state,
            s.agent_id.to_hex(),
            "tunnel_ice",
        );
    }
}

/// Relay the tunnel-client's QUIC relay candidate(s) to the agent so it
/// can install a TURN permission for the client's relay address before
/// the QUIC handshake (Phase 3c). Mirror of [`relay_ice_to_agent`].
async fn relay_quic_candidate_to_agent(
    state: &NetworkState,
    session: Option<&TunnelSession>,
    request_session_id: ObjectId,
    addrs: Vec<String>,
) {
    let Some(s) = session else {
        return;
    };
    if s.tunnel_session_id != request_session_id {
        return;
    }
    if let Err(e) = state.fleet.rc_hub.send_to_agent(
        s.agent_id,
        ServerMsg::TunnelQuicCandidate {
            session_id: request_session_id,
            addrs,
        },
    ) {
        debug!(%request_session_id, %e, "tunnel QUIC candidate relay to agent failed");
    }
}

/// Append the flow-close audit row.
///
/// `bytes` = `(in, out)` **as the reporting endpoint counted them**. The
/// server cannot measure this itself — tunnel payload rides the peer-to-peer
/// data channel — which is why these two columns held a literal 0 on every
/// row written before wave 3, and why the tunnel byte series was flat.
/// `(0, 0)` from a pre-wave-3 client is therefore still the honest answer:
/// nobody counted.
async fn audit_tcp_close(
    state: &NetworkState,
    session: &TunnelSession,
    orig: &Originator,
    flow_id: u32,
    reason: CloseReason,
    bytes: (u64, u64),
) {
    let _ = state
        .tunnel_audit
        .append(&TunnelAuditEvent {
            id: None,
            tenant_id: session.agent_tenant_id,
            tunnel_session_id: session.tunnel_session_id,
            tunnel_client_id: orig.tunnel_client_id(),
            origin_agent_id: orig.origin_agent_id(),
            agent_id: session.agent_id,
            user_id: orig.owner_user_id,
            at: DateTime::now(),
            kind: TunnelAuditKind::TcpClosed,
            flow_id: Some(flow_id),
            dst_host: None,
            dst_port: None,
            bytes_in: bytes.0,
            bytes_out: bytes.1,
            message_count: 0,
            duration_ms: None,
            relay: RelayMode::Direct,
            client_src_ip: None,
            agent_src_port: None,
            client_version: orig.client_version.clone(),
            client_os: orig.client_os,
            reason: Some(format!("{reason:?}")),
        })
        .await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Revocation re-check (lifted from T1 stub, now sends typed ServerMsg)
// ─────────────────────────────────────────────────────────────────────────────

/// The tunnel-client connection loop's end signal: the server ends this
/// connection itself. Sets `orig.server_ended`, so the client's ack and the
/// teardown also ask the exit to close, and returns the terminate the
/// session's exit gets at once: where to send it and the frame. `None` when
/// the connection holds no session.
fn exit_end(orig: &Originator, session: Option<&TunnelSession>) -> Option<(ObjectId, ServerMsg)> {
    orig.server_ended.set();
    session.map(|s| {
        (
            s.agent_id,
            orig.exit_terminate(s.tunnel_session_id, CloseReason::ServerTerminated),
        )
    })
}

/// The check's half of the end signal. Sets `server_ended` first, so the
/// client's ack and the teardown ask the exit to close whenever the loop
/// gets to them, then hands the loop the signal and waits for it to have
/// told the session's exit, at most `within`. Returns whether it did.
async fn signal_end(
    server_ended: &roomler_core::ServerEnded,
    end_tx: &mpsc::Sender<oneshot::Sender<()>>,
    within: Duration,
) -> bool {
    server_ended.set();
    let told = async {
        let (told_tx, told_rx) = oneshot::channel();
        end_tx.send(told_tx).await.ok()?;
        told_rx.await.ok()
    };
    matches!(tokio::time::timeout(within, told).await, Ok(Some(())))
}

/// #4571 — the server's keepalive Ping on a tunnel client's socket, from the
/// write side as the DERP socket's write task sends its own (`derp.rs`):
/// through the socket's one sink, which the pump and the 60 s check also write
/// through. A Ping that cannot be written, to a client that has gone silent,
/// holds this task and the sink but never the read loop, whose deadline then
/// ends the socket. A client answers each Ping while it reads (tungstenite
/// does it), so every released `roomler` stays inside that deadline.
fn spawn_keepalive(
    socket_tx: Arc<Mutex<SplitSink<WebSocket, Message>>>,
    every: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick is now: the first Ping goes one period in.
        tick.tick().await;
        loop {
            tick.tick().await;
            let mut sink = socket_tx.lock().await;
            if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                return;
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn spawn_revocation_check(
    state: NetworkState,
    socket_tx: Arc<Mutex<SplitSink<WebSocket, Message>>>,
    outbound_tx: mpsc::Sender<ServerMsg>,
    end_tx: mpsc::Sender<oneshot::Sender<()>>,
    server_ended: roomler_core::ServerEnded,
    tunnel_client_id: ObjectId,
    tenant_id: ObjectId,
    presence: PresenceRecord,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(REVOCATION_CHECK_INTERVAL);
        tick.tick().await;
        loop {
            tick.tick().await;
            // ANY, deliberately (#1829): this read's job IS the tombstone.
            // `deleted_at` decides between the heartbeat arm and the typed
            // `rc:tunnel.revoked` kick below, while the `Err` arm means "row
            // unreadable" and keeps the socket OPEN (a Mongo blip must not
            // revoke a healthy fleet). A LIVE lookup would fold a removal
            // into that `Err` arm — and a deleted client would never be
            // kicked, for as long as its session lived.
            match state
                .tunnel_clients
                .find_any_in_tenant(tenant_id, tunnel_client_id)
                .await
            {
                Ok(c)
                    if c.deleted_at.is_none()
                        && matches!(c.status, AgentStatus::Online | AgentStatus::Offline) =>
                {
                    let _ = state.tunnel_clients.touch_heartbeat(tunnel_client_id).await;
                    presence.refresh().await;
                }
                Ok(_) => {
                    info!(%tunnel_client_id, "tunnel-client revoked mid-session; closing WS");
                    // The connection loop tells the session's exit first.
                    signal_end(&server_ended, &end_tx, END_TOLD_WITHIN).await;
                    send_msg(
                        &outbound_tx,
                        ServerMsg::TunnelRevoked {
                            reason: "status changed to Quarantined or soft-deleted".into(),
                        },
                    )
                    .await;
                    // Give the pump a moment to flush the revocation
                    // message before slamming the socket shut.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let mut guard = socket_tx.lock().await;
                    let _ = guard.close().await;
                    return;
                }
                Err(e) => {
                    warn!(
                        %tunnel_client_id, %e,
                        "tunnel-client revocation re-check lookup failed; keeping connection open"
                    );
                    // #4553 — the socket stays open, so its presence does.
                    presence.refresh().await;
                }
            }
        }
    })
}

/// #4553 — a tunnel client's presence on this pod, as its sockets write it,
/// read by `fleet::agent::derive_tunnel_client_presence` as an agent's is
/// (Phase A-1): each socket's seat in this pod's registry and, on a pod with
/// a directory, the client's cross-pod record (`directory::tunnel_client_key`,
/// TTL `TUNNEL_CLIENT_TTL_SECS`).
///
/// The record follows the directory's rules for a connection-bound entity,
/// with the client's sockets on this pod as ONE registration: they share one
/// token ([`TunnelClientSockets`]), as an agent's hub slot holds one.
/// - claimed at connect with `claim_if_newer`: newest wins, by the time each
///   pod minted its token, so a socket made later on another pod keeps the
///   record however the two writes land;
/// - refreshed by the 60 s check with `refresh_if_mine`, the token read live:
///   ours or gone is set again (a record lost to a Redis blip heals within one
///   check), another pod's is left alone;
/// - released compare-and-delete when the client's LAST socket here closes,
///   so an old pod's late close never takes a newer socket's record with it.
///
/// `status` is never written: it is the admin's, and with two pods a
/// disconnect write could land after a reconnect on the other pod.
#[derive(Clone)]
struct PresenceRecord {
    sockets: Arc<TunnelClientSockets>,
    client: ObjectId,
    /// `None` on a pod with no directory: there the seat alone answers, and
    /// only on this pod.
    dir: Option<OwnershipDirectory>,
    key: String,
}

impl PresenceRecord {
    /// Seat one socket of `client` on this pod and claim the record with the
    /// client's token here.
    async fn open(state: &NetworkState, client: ObjectId) -> (TunnelClientSeat, Self) {
        let record = Self {
            sockets: state.fleet.tunnel_client_sockets.clone(),
            client,
            dir: state.cluster_directory.clone(),
            key: tunnel_client_key(&client.to_hex()),
        };
        let (seat, token) = record.sockets.sit(client, || {
            record
                .dir
                .as_ref()
                .map(|dir| dir.owner_token(&ObjectId::new().to_hex()))
        });
        if let (Some(dir), Some(token)) = (&record.dir, &token) {
            match dir
                .claim_if_newer(&record.key, token, TUNNEL_CLIENT_TTL_SECS)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    debug!(%client, "tunnel-client presence record already holds this token or a newer one")
                }
                Err(e) => warn!(
                    %client, %e,
                    "tunnel-client presence claim failed; the 60 s check claims it again"
                ),
            }
        }
        (seat, record)
    }

    /// The 60 s check's refresh. Nothing to write on a pod with no directory,
    /// or once the shutdown release took the token.
    async fn refresh(&self) {
        let (Some(dir), Some(token)) = (&self.dir, self.sockets.token(self.client)) else {
            return;
        };
        match dir
            .refresh_if_mine(&self.key, &token, TUNNEL_CLIENT_TTL_SECS)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                debug!(client = %self.client, "another pod holds this tunnel client's presence record")
            }
            Err(e) => debug!(client = %self.client, %e, "tunnel-client presence refresh failed"),
        }
    }

    /// The socket's end: its seat leaves. If it was the client's last socket
    /// on this pod, the record is released if it is still ours.
    async fn close(&self, seat: TunnelClientSeat) {
        let Some(token) = seat.leave() else {
            return;
        };
        if let Some(dir) = &self.dir
            && let Err(e) = dir.release(&self.key, &token).await
        {
            debug!(client = %self.client, %e, "tunnel-client presence release failed; its TTL ends the record");
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// P3b-2: an agent driving the tunnel-CLIENT role over its own agent WS
// ─────────────────────────────────────────────────────────────────────────────

/// Agent-WS interception for the tunnel-CLIENT role (P3b-2, identity model
/// (b)). An enrolled agent may ORIGINATE tunnel-client sessions over its
/// own agent WS. This runs FIRST in the agent read-loop — ahead of the
/// target-side [`crate::agent_arms::relay_tunnel_msg_from_agent`] —
/// and consumes the variants where THIS agent acts as the tunnel
/// *originator*, driving the same subject-agnostic handlers with
/// `Principal::Agent`.
///
/// Returns `None` when consumed; `Some(parsed)` to pass the message on to
/// the target-side handler (this agent is the tunnel TARGET for that
/// session, not its originator).
///
/// Demux (P3b-2 Risk 1). The client-only-by-type variants
/// (`TunnelHello` / `TunnelOpen` / `Tcp|UdpForwardRequest` /
/// `TunnelSdpOffer` / `TunnelQuicCandidate`) are always originator-side
/// here — an agent never emits them as a target. The genuinely
/// bidirectional ones (`TunnelIce` / `TunnelTerminate` / `TcpHalfClose` /
/// `TcpClosed` / `UdpClosed`) are consumed ONLY for a session THIS agent
/// originated (`session_id ∈ sessions`); otherwise they fall through so
/// the existing target-side path relays the agent's answer back to the far
/// originating client. The reply path for an originated session reuses
/// `tunnel_clients_by_session[session_id]` — registered on open with the
/// agent's OWN outbound channel — so a target's answers reach this agent's
/// socket unchanged.
pub async fn relay_tunnel_client_msg_from_agent(
    state: &NetworkState,
    orig: &Originator,
    sessions: &mut std::collections::HashMap<ObjectId, TunnelSession>,
    supported_transports: &mut Vec<String>,
    parsed: ClientMsg,
) -> Option<ClientMsg> {
    match parsed {
        ClientMsg::TunnelHello {
            role: _,
            version,
            supported_transports: advertised,
        } => {
            debug!(agent = %orig.log_id(), %version, ?advertised, "agent tunnel-client hello");
            *supported_transports = advertised;
            None
        }
        ClientMsg::TunnelOpen {
            agent_id,
            transport,
            open_nonce,
            derp_pubkey,
            net_grant,
        } => {
            if let Some(s) = handle_tunnel_open(
                state,
                orig,
                agent_id,
                transport,
                open_nonce,
                derp_pubkey,
                // FR-137 S-B3 — refused if present.
                net_grant,
                supported_transports,
            )
            .await
            {
                // PR-1 — mirror into the by-ORIGIN index (the per-conn
                // map below is invisible to the nudge busy check; without
                // this a routes-only agent read as idle and got cycled).
                if let Some(origin_agent) = orig.origin_agent_id() {
                    state
                        .tunnel_sessions_by_origin_agent
                        .entry(origin_agent)
                        .or_default()
                        .insert(s.tunnel_session_id);
                }
                sessions.insert(s.tunnel_session_id, s);
            }
            None
        }
        ClientMsg::TcpForwardRequest {
            session_id,
            flow_id,
            dst_host,
            dst_port,
        } => {
            handle_forward_request(
                ProtocolKind::Tcp,
                state,
                orig,
                sessions.get(&session_id),
                session_id,
                flow_id,
                &dst_host,
                dst_port,
            )
            .await;
            None
        }
        ClientMsg::UdpForwardRequest {
            session_id,
            flow_id,
            dst_host,
            dst_port,
        } => {
            handle_forward_request(
                ProtocolKind::Udp,
                state,
                orig,
                sessions.get(&session_id),
                session_id,
                flow_id,
                &dst_host,
                dst_port,
            )
            .await;
            None
        }
        ClientMsg::TunnelSdpOffer { session_id, sdp } => {
            relay_sdp_offer_to_agent(state, sessions.get(&session_id), session_id, sdp).await;
            None
        }
        ClientMsg::TunnelQuicCandidate { session_id, addrs } => {
            relay_quic_candidate_to_agent(state, sessions.get(&session_id), session_id, addrs)
                .await;
            None
        }
        // ── genuinely bidirectional: consume only if WE originated it ──
        ClientMsg::TunnelIce {
            session_id,
            candidate,
        } => {
            if sessions.contains_key(&session_id) {
                relay_ice_to_agent(state, sessions.get(&session_id), session_id, candidate).await;
                None
            } else {
                Some(ClientMsg::TunnelIce {
                    session_id,
                    candidate,
                })
            }
        }
        ClientMsg::TcpHalfClose {
            session_id,
            flow_id,
            direction,
        } => {
            if sessions.contains_key(&session_id) {
                relay_half_close_to_agent(
                    state,
                    sessions.get(&session_id),
                    orig.log_id(),
                    session_id,
                    flow_id,
                    direction,
                )
                .await;
                None
            } else {
                Some(ClientMsg::TcpHalfClose {
                    session_id,
                    flow_id,
                    direction,
                })
            }
        }
        ClientMsg::TcpClosed {
            session_id,
            flow_id,
            reason,
            bytes_in,
            bytes_out,
        } => {
            if sessions.contains_key(&session_id) {
                relay_tcp_closed_to_agent(
                    state,
                    orig,
                    sessions.get(&session_id),
                    session_id,
                    flow_id,
                    reason,
                    (bytes_in, bytes_out),
                )
                .await;
                None
            } else {
                Some(ClientMsg::TcpClosed {
                    session_id,
                    flow_id,
                    reason,
                    bytes_in,
                    bytes_out,
                })
            }
        }
        ClientMsg::UdpClosed {
            session_id,
            flow_id,
            reason,
        } => {
            if sessions.contains_key(&session_id) {
                relay_udp_closed_to_agent(
                    state,
                    orig,
                    sessions.get(&session_id),
                    session_id,
                    flow_id,
                    reason,
                )
                .await;
                None
            } else {
                Some(ClientMsg::UdpClosed {
                    session_id,
                    flow_id,
                    reason,
                })
            }
        }
        ClientMsg::TunnelTerminate { session_id, reason } => {
            if let Some(s) = sessions.remove(&session_id) {
                // We originated this session — tear it down like the
                // tunnel-client socket's terminate arm: tell the target
                // agent, drop the reply-channel registration, audit close.
                let _ = state
                    .fleet
                    .rc_hub
                    .send_to_agent(s.agent_id, orig.exit_terminate(s.tunnel_session_id, reason));
                state.tunnel_clients_by_session.remove(&s.tunnel_session_id);
                unindex_session_target(
                    &state.tunnel_sessions_by_target_agent,
                    s.agent_id,
                    s.tunnel_session_id,
                );
                if let Some(origin_agent) = orig.origin_agent_id() {
                    unindex_session_target(
                        &state.tunnel_sessions_by_origin_agent,
                        origin_agent,
                        s.tunnel_session_id,
                    );
                }
                audit_peer_close(state, &s, orig).await;
                None
            } else {
                Some(ClientMsg::TunnelTerminate { session_id, reason })
            }
        }
        // Everything else — target-side answers + non-tunnel rc:* — passes
        // through to the target-side interception / Hub.
        other => Some(other),
    }
}

/// Tear down every tunnel session an agent originated when its WS drops.
/// Mirrors the tunnel-client socket's disconnect path: for each session,
/// tell the target agent, drop the reply-channel registration, and audit
/// the close. Consumes the map.
pub async fn teardown_agent_originated_sessions(
    state: &NetworkState,
    orig: &Originator,
    sessions: std::collections::HashMap<ObjectId, TunnelSession>,
) {
    for (_session_id, s) in sessions {
        state.tunnel_clients_by_session.remove(&s.tunnel_session_id);
        unindex_session_target(
            &state.tunnel_sessions_by_target_agent,
            s.agent_id,
            s.tunnel_session_id,
        );
        // PR-1 — the by-ORIGIN twin (same shape, keyed by the closing
        // originator; see the TunnelOpen relay arm's mirror insert).
        if let Some(origin_agent) = orig.origin_agent_id() {
            unindex_session_target(
                &state.tunnel_sessions_by_origin_agent,
                origin_agent,
                s.tunnel_session_id,
            );
        }
        let _ = state.fleet.rc_hub.send_to_agent(
            s.agent_id,
            orig.exit_terminate(s.tunnel_session_id, CloseReason::ClientShutdown),
        );
        audit_peer_close(state, &s, orig).await;
    }
}

/// Drop one session from the by-target-agent index (see
/// `NetworkState::tunnel_sessions_by_target_agent`), tidying away the agent's
/// entry once its set is empty. The guard is dropped before `remove_if` so
/// the two DashMap operations never overlap on the same shard.
fn unindex_session_target(
    index: &dashmap::DashMap<ObjectId, std::collections::HashSet<ObjectId>>,
    agent_id: ObjectId,
    session_id: ObjectId,
) {
    if let Some(mut set) = index.get_mut(&agent_id) {
        set.remove(&session_id);
    }
    index.remove_if(&agent_id, |_, set| set.is_empty());
}

/// P7 flap resilience: when an agent's WS drops, every tunnel session
/// TARGETING it is dead — the agent's per-connection tunnel peers died with
/// its socket, so even after it reconnects it will reject every forward on
/// those sessions with "tunnel session not open on agent", forever. Push
/// `rc:tunnel.terminate` to each such session's client so its flow
/// supervisor re-opens promptly instead of discovering the corpse one
/// rejected flow at a time. Clients ack with their own `TunnelTerminate`,
/// which runs the normal per-connection teardown + audit; the global
/// reply-channel entry is left for that ack path (pre-P7 clients without
/// the ack still end their session on our terminate, and their entry is
/// swept on their own disconnect, exactly as today).
pub async fn terminate_sessions_targeting_agent(state: &NetworkState, agent_id: ObjectId) {
    // R3 — optional grace before terminating sessions when the target agent's
    // control WS drops. `0` (default) = terminate immediately (pre-R3). `>0` =
    // keep the sessions (their QUIC data plane + the nudge busy-gate index rely
    // on it) and terminate only if the agent is STILL offline after the window.
    // A same-pod re-register (hub) or cross-pod one (fresh foreign Redis
    // presence) cancels it — no explicit cancel registry needed. Pairs with the
    // agent's `tunnel_peers_survive_reattach`.
    let grace = state.settings.rc.tunnel_grace_secs;
    if grace == 0 {
        terminate_agent_sessions_now(state, agent_id).await;
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(grace)).await;
        if state.fleet.rc_hub.is_agent_online(agent_id) {
            debug!(%agent_id, "R3 grace: agent re-registered on this pod — tunnel sessions kept");
            return;
        }
        // A fresh FOREIGN presence record ⇒ the agent re-registered on another
        // pod; keep the sessions (its reclaimed peers serve them). A wedged or
        // unreachable Redis falls through to terminate — fail-safe: never keep a
        // session alive on an unverifiable presence — bounded by the same 250 ms
        // as the open-time rehome probe.
        if let Some(redis) = &state.redis_pubsub
            && let Ok(Ok(Some(_))) = tokio::time::timeout(
                std::time::Duration::from_millis(250),
                redis.agent_presence_foreign(&agent_id.to_hex()),
            )
            .await
        {
            debug!(%agent_id, "R3 grace: agent re-registered on another pod — tunnel sessions kept");
            return;
        }
        info!(%agent_id, grace_secs = grace, "R3 grace expired — agent still offline; terminating tunnel sessions");
        terminate_agent_sessions_now(&state, agent_id).await;
    });
}

/// Immediately terminate every tunnel session targeting `agent_id`: drop the
/// by-target index and push `TunnelTerminate` to each client. The pre-R3
/// behaviour, now also the grace-expiry path.
async fn terminate_agent_sessions_now(state: &NetworkState, agent_id: ObjectId) {
    let Some((_, session_ids)) = state.tunnel_sessions_by_target_agent.remove(&agent_id) else {
        return;
    };
    for sid in session_ids {
        // Clone the tx out of the map guard before awaiting — holding a
        // DashMap ref across an await point can deadlock the shard.
        let tx = state
            .tunnel_clients_by_session
            .get(&sid)
            .map(|e| e.value().clone());
        if let Some(tx) = tx {
            info!(%agent_id, session_id = %sid, "target agent WS dropped — terminating tunnel session");
            // try_send, not send: this runs on the AGENT's WS teardown path,
            // and an awaited send on a wedged client channel (blackholed
            // socket, full cap-256 buffer) would stall unregister +
            // mark_status(offline) behind an unrelated client. A dropped
            // push is fine — the client still heals via the session-gone
            // reject on its next forward.
            let msg = ServerMsg::TunnelTerminate {
                session_id: sid,
                reason: CloseReason::ServerTerminated,
                close_connection: false,
            };
            if let Err(e) = tx.try_send(msg) {
                debug!(%agent_id, session_id = %sid, %e, "terminate push dropped (client channel full/closed)");
            }
        }
    }
}

/// The `/ws?role=tunnel-client` upgrade (FR-69 P7b, from the host): verify the
/// tunnel-client JWT, the affinity key and the row, say why on refusal, then
/// run the loop. The host keeps the `/ws` route and the role gate.
pub fn ws_upgrade_tunnel_client(
    state: NetworkState,
    token: String,
    tid: Option<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let claims = match state.auth.verify_tunnel_client_token(&token) {
        Ok(c) => c,
        Err(_) => {
            return Response::builder()
                .status(401)
                .body("Unauthorized (tunnel-client)".into())
                .unwrap();
        }
    };

    // S6 — a present affinity key must match the token's tenant.
    if !tid_matches_claim(&tid, &claims.tenant_id) {
        return Response::builder()
            .status(403)
            .body("tid does not match token tenant".into())
            .unwrap();
    }

    let tunnel_client_id = match ObjectId::parse_str(&claims.sub) {
        Ok(id) => id,
        Err(_) => {
            return Response::builder()
                .status(400)
                .body("Invalid tunnel-client ID".into())
                .unwrap();
        }
    };
    let tenant_id = match ObjectId::parse_str(&claims.tenant_id) {
        Ok(id) => id,
        Err(_) => {
            return Response::builder()
                .status(400)
                .body("Invalid tenant ID".into())
                .unwrap();
        }
    };
    let owner_user_id = match ObjectId::parse_str(&claims.owner_user_id) {
        Ok(id) => id,
        Err(_) => {
            return Response::builder()
                .status(400)
                .body("Invalid owner user ID".into())
                .unwrap();
        }
    };
    let token_gen = claims.generation;

    ws.max_message_size(MAX_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
        // Connect-time revocation check. Periodic re-check (every 60 s)
        // lives in `ws::tunnel::handle_tunnel_client_socket`.
        //
        // ANY, deliberately (#1829): the tombstone is what this read is FOR
        // — `deleted_at` below selects the typed `rc:tunnel.revoked` goodbye
        // (close 4003, "re-enrol to revive") the CLI logs and stops on. A
        // LIVE lookup would turn a removal into the `Err` arm's frameless
        // `return`, and the CLI would reconnect forever against what reads
        // as a network fault.
        let client = match state
            .tunnel_clients
            .find_any_in_tenant(tenant_id, tunnel_client_id)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                warn!(%tunnel_client_id, %e, "tunnel-client lookup failed on WS connect");
                return;
            }
        };
        if client.deleted_at.is_some()
            || matches!(
                client.status,
                roomler_ai_remote_control::models::AgentStatus::Quarantined
            )
        {
            // rc.53: mirror of the agent refusal path. Tunnel-clients
            // have their own taxonomy (`ServerMsg::TunnelRevoked`) and
            // the periodic re-check in `handle_tunnel_client_socket`
            // already emits it on mid-session revocation — extend the
            // same notification to the connect-time refusal so the CLI
            // logs "tunnel revoked" instead of opaque socket close.
            info!(%tunnel_client_id, "tunnel-client is quarantined or deleted; refusing WS with rc:tunnel.revoked");
            let revoked = roomler_ai_remote_control::signaling::ServerMsg::TunnelRevoked {
                reason: "tunnel-client row was deleted or quarantined; re-enrol to revive".into(),
            };
            send_goodbye_and_close(socket, &revoked, 4003, "tunnel_client_deleted").await;
            return;
        }
        // FR-103 — a token minted before this client was removed and
        // re-enrolled. To the CLI that is a revoked credential: it stops and
        // says to re-enroll, which is exactly the remedy.
        if let Some(reason) = roomler_ai_mod_fleet::auth_agent::generation_refusal(
            token_gen,
            client.token_gen,
            false, // a tunnel client does not rotate (FR-155)
            state.fleet.settings.rc.token_generation_check,
        ) {
            info!(%tunnel_client_id, reason, row_gen = client.token_gen, "tunnel-client token predates its removal; refusing WS with rc:tunnel.revoked");
            let revoked = roomler_ai_remote_control::signaling::ServerMsg::TunnelRevoked {
                reason: reason.into(),
            };
            send_goodbye_and_close(socket, &revoked, 4003, "tunnel_client_superseded").await;
            return;
        }
        handle_tunnel_client_socket(
            state,
            socket,
            tunnel_client_id,
            tenant_id,
            owner_user_id,
            tid,
        )
        .await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use tunnel_core::policy::{
        Decision, DestinationRule, HostPattern, PolicyTarget, PortRange, evaluate,
    };

    // ─── #4542: which terminates ask the exit to close the connection ──

    fn originator() -> Originator {
        let (outbound_tx, _rx) = mpsc::channel(1);
        Originator {
            principal: Principal::TunnelClient(ObjectId::new()),
            tenant_id: ObjectId::new(),
            owner_user_id: ObjectId::new(),
            client_version: String::new(),
            client_os: OsKind::Linux,
            outbound_tx,
            dialed_tid: None,
            conn_established_ms: 0,
            server_ended: roomler_core::ServerEnded::default(),
        }
    }

    fn quic_session(exit: ObjectId) -> TunnelSession {
        TunnelSession {
            tunnel_session_id: ObjectId::new(),
            agent_id: exit,
            agent_tenant_id: ObjectId::new(),
            transport: TRANSPORT_QUIC_V1.into(),
        }
    }

    /// `(session_id, reason, close_connection)` of an `rc:tunnel.terminate`.
    fn terminate_fields(msg: ServerMsg) -> (ObjectId, CloseReason, bool) {
        match msg {
            ServerMsg::TunnelTerminate {
                session_id,
                reason,
                close_connection,
            } => (session_id, reason, close_connection),
            other => panic!("expected rc:tunnel.terminate, got {other:?}"),
        }
    }

    #[test]
    fn an_exit_terminate_asks_to_close_only_once_the_server_ended_its_originator() {
        let orig = originator();
        let sid = ObjectId::new();
        for reason in [CloseReason::ClientShutdown, CloseReason::ServerTerminated] {
            assert_eq!(
                terminate_fields(orig.exit_terminate(sid, reason)),
                (sid, reason, false),
                "an originator the server has not ended relays and tears down unmarked"
            );
        }

        // A clone shares the flag, as the agent socket's clones do.
        orig.clone().server_ended.set();

        for reason in [CloseReason::ClientShutdown, CloseReason::ServerTerminated] {
            assert_eq!(
                terminate_fields(orig.exit_terminate(sid, reason)),
                (sid, reason, true),
                "once the server ended the originator, every terminate to an exit asks to close"
            );
        }
    }

    #[test]
    fn the_end_signal_tells_the_sessions_exit_to_close_and_so_does_what_follows() {
        let orig = originator();
        let exit = ObjectId::new();
        let s = quic_session(exit);

        let (to, msg) = exit_end(&orig, Some(&s)).expect("the session's exit is told");

        assert_eq!(to, exit, "the terminate goes to the session's exit");
        assert_eq!(
            terminate_fields(msg),
            (s.tunnel_session_id, CloseReason::ServerTerminated, true),
            "the end signal's terminate asks the exit to close"
        );
        assert!(
            terminate_fields(
                orig.exit_terminate(s.tunnel_session_id, CloseReason::ServerTerminated)
            )
            .2,
            "the client's ack, relayed, asks the same"
        );
        assert!(
            terminate_fields(orig.exit_terminate(s.tunnel_session_id, CloseReason::ClientShutdown))
                .2,
            "the teardown asks the same"
        );
    }

    #[test]
    fn the_end_signal_without_a_session_tells_no_exit() {
        assert!(exit_end(&originator(), None).is_none());
    }

    #[tokio::test]
    async fn the_end_signal_returns_once_the_loop_told_the_exit() {
        let ended = roomler_core::ServerEnded::default();
        let (end_tx, mut end_rx) = mpsc::channel::<oneshot::Sender<()>>(1);
        let connection_loop = tokio::spawn(async move {
            let told = end_rx.recv().await.expect("the loop gets the signal");
            told.send(()).expect("the check waits for the loop");
        });

        assert!(
            signal_end(&ended, &end_tx, Duration::from_secs(5)).await,
            "the check hears that the loop told the exit"
        );
        assert!(ended.get(), "the connection reads as ended by the server");
        connection_loop.await.unwrap();
    }

    #[tokio::test]
    async fn a_loop_that_does_not_answer_holds_the_end_no_longer_than_its_bound() {
        let ended = roomler_core::ServerEnded::default();
        let (end_tx, _end_rx) = mpsc::channel::<oneshot::Sender<()>>(1);
        let started = std::time::Instant::now();

        let told = tokio::time::timeout(
            Duration::from_secs(5),
            signal_end(&ended, &end_tx, Duration::from_millis(200)),
        )
        .await
        .expect("the end goes on once the bound passes");

        assert!(!told, "the loop did not tell the exit");
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert!(
            ended.get(),
            "the client's ack and the teardown still ask the exit to close"
        );
    }

    #[tokio::test]
    async fn a_loop_that_ended_does_not_hold_the_end() {
        let ended = roomler_core::ServerEnded::default();
        let (end_tx, end_rx) = mpsc::channel::<oneshot::Sender<()>>(1);
        drop(end_rx);

        let told = tokio::time::timeout(
            Duration::from_secs(5),
            signal_end(&ended, &end_tx, Duration::from_secs(60)),
        )
        .await
        .expect("the end goes on at once");

        assert!(!told);
        assert!(ended.get());
    }

    #[test]
    fn no_terminate_this_module_sends_an_exit_is_built_by_hand() {
        fn code(src: &str) -> &str {
            &src[..src.find("#[cfg(test)]").unwrap_or(src.len())]
        }
        for (file, src) in [
            ("tunnel.rs", code(include_str!("tunnel.rs"))),
            ("agent_arms.rs", code(include_str!("agent_arms.rs"))),
        ] {
            for (at, _) in src.match_indices("send_to_agent(") {
                let call = &src[at..];
                let call = &call[..call.find(";").unwrap_or(call.len())];
                assert!(
                    !call.contains("TunnelTerminate"),
                    "{file}: a terminate an exit gets is built by Originator::exit_terminate: {call}"
                );
            }
        }
    }

    // ─── #4571: the keepalive and the receive deadline ──

    /// A deployment pings every 30 s and ends a socket silent for 90 s, the
    /// client's own keepalive and deadline. Only a test's period replaces
    /// them, and the deadline stays three of its periods.
    #[test]
    fn a_deployment_pings_every_30_s_and_ends_a_socket_silent_for_90_s() {
        let mut rc = roomler_ai_config::RcSettings::default();
        assert_eq!(
            keepalive(&rc),
            (Duration::from_secs(30), Duration::from_secs(90))
        );
        rc.tunnel_ping_every = Some(Duration::from_millis(400));
        assert_eq!(
            keepalive(&rc),
            (Duration::from_millis(400), Duration::from_millis(1200))
        );
    }

    /// A policy granting `127.0.0.1:port` on every device to `subjects`.
    fn policy(subjects: Vec<PolicySubject>, port: u16) -> TunnelPolicy {
        let now = DateTime::now();
        TunnelPolicy {
            id: Some(ObjectId::new()),
            tenant_id: ObjectId::new(),
            name: format!("grant :{port}"),
            subjects,
            targets: vec![PolicyTarget::AllAgents],
            allowlist: vec![DestinationRule {
                host_pattern: HostPattern::Cidr("127.0.0.0/8".into()),
                port_range: PortRange {
                    low: port,
                    high: port,
                },
                proto: ProtocolKind::Any,
            }],
            max_concurrent_flows: None,
            max_bytes_per_session: None,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        }
    }

    fn role(role_id: ObjectId) -> PolicySubject {
        PolicySubject::RoleId { role_id }
    }

    /// [`forward_role_ids`] over `policies` with a lookup that answers
    /// `answer`, and how many times the lookup ran.
    fn resolve(
        policies: &[TunnelPolicy],
        answer: Result<Vec<ObjectId>, &'static str>,
    ) -> (Vec<ObjectId>, u32) {
        let calls = Cell::new(0);
        let lookup = || {
            calls.set(calls.get() + 1);
            std::future::ready(answer)
        };
        let roles =
            futures::executor::block_on(forward_role_ids(policies, lookup, ObjectId::new()));
        (roles, calls.get())
    }

    /// Whether `evaluate` lets `subject` reach `127.0.0.1:port` on some device.
    fn allows(policies: &[TunnelPolicy], subject: &ResolvedSubject, port: u16) -> bool {
        matches!(
            evaluate(
                policies,
                subject,
                ObjectId::new(),
                "127.0.0.1",
                port,
                ProtocolKind::Tcp
            ),
            Decision::Allow { .. }
        )
    }

    /// A `tracing` writer into a buffer, so a test reads the WARN line the
    /// gate writes. Installed thread-scoped (`with_default`), so a parallel
    /// test's events never land in it.
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Captured {
            self.clone()
        }
    }

    /// Run `f` with every WARN+ event captured; return its value and the text.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test's own log capture, never a console (root clippy.toml, #3938)"
    )]
    fn capture_warns<T>(f: impl FnOnce() -> T) -> (T, String) {
        let buf = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        let out = tracing::subscriber::with_default(subscriber, f);
        let text = String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned();
        (out, text)
    }

    /// No live policy names a role: no role list can change the decision, so
    /// none is read.
    #[test]
    fn without_a_role_subject_the_roles_are_not_read() {
        let others = vec![
            policy(
                vec![PolicySubject::UserId {
                    user_id: ObjectId::new(),
                }],
                9001,
            ),
            policy(
                vec![PolicySubject::TunnelClientId {
                    tunnel_client_id: ObjectId::new(),
                }],
                9002,
            ),
            policy(
                vec![PolicySubject::AgentId {
                    agent_id: ObjectId::new(),
                }],
                9003,
            ),
            policy(vec![PolicySubject::AllUsers], 9004),
        ];
        assert_eq!(resolve(&others, Ok(vec![ObjectId::new()])), (vec![], 0));
        assert_eq!(resolve(&[], Ok(vec![ObjectId::new()])), (vec![], 0));
    }

    /// A policy the evaluator skips (deleted) names no role either.
    #[test]
    fn a_deleted_role_policy_reads_nothing() {
        let mut gone = policy(vec![role(ObjectId::new())], 9000);
        gone.deleted_at = Some(DateTime::now());
        assert_eq!(resolve(&[gone], Ok(vec![ObjectId::new()])), (vec![], 0));
    }

    /// A live role subject: the roles are read once, returned as read, and
    /// decide the role policy: a holder is allowed, anyone else is not.
    #[test]
    fn a_role_subject_reads_the_roles_and_they_decide_the_policy() {
        let ops = ObjectId::new();
        let other = ObjectId::new();
        let policies = vec![policy(vec![role(ops)], 9000)];

        let (held, calls) = resolve(&policies, Ok(vec![other, ops]));
        assert_eq!((held.clone(), calls), (vec![other, ops], 1));
        let holder = ResolvedSubject {
            user_id: ObjectId::new(),
            role_ids: held,
            principal: Principal::TunnelClient(ObjectId::new()),
        };
        assert!(allows(&policies, &holder, 9000), "a holder of the role");

        let (none_held, _) = resolve(&policies, Ok(vec![other]));
        let not_holder = ResolvedSubject {
            role_ids: none_held,
            ..holder
        };
        assert!(!allows(&policies, &not_holder, 9000), "a person without it");
    }

    /// The read fails: no roles, a WARN that names the origin and the error,
    /// and never an allow through a role. A user subject in the same set still
    /// allows its user: only role subjects lose.
    #[test]
    fn an_unreadable_role_list_is_no_roles_and_a_warn_never_an_allow() {
        let person = ObjectId::new();
        let origin = ObjectId::new();
        let ops = ObjectId::new();
        let policies = vec![
            policy(vec![role(ops)], 9000),
            policy(vec![PolicySubject::UserId { user_id: person }], 9001),
        ];
        let calls = Cell::new(0);
        let (roles, warns) = capture_warns(|| {
            futures::executor::block_on(forward_role_ids(
                &policies,
                || {
                    calls.set(calls.get() + 1);
                    std::future::ready(Err::<Vec<ObjectId>, _>("role store unreachable"))
                },
                origin,
            ))
        });

        assert_eq!((roles.clone(), calls.get()), (vec![], 1));
        assert!(warns.contains("WARN"), "a WARN is written: {warns}");
        assert!(warns.contains("roles unreadable"), "{warns}");
        assert!(
            warns.contains(&origin.to_hex()),
            "the WARN names the origin: {warns}"
        );
        assert!(
            warns.contains("role store unreachable"),
            "and the error: {warns}"
        );

        let subject = ResolvedSubject {
            user_id: person,
            role_ids: roles,
            principal: Principal::Agent(origin),
        };
        assert!(
            !allows(&policies, &subject, 9000),
            "an unreadable role list never allows through a role"
        );
        assert!(
            allows(&policies, &subject, 9001),
            "a user subject in the same set evaluates as before"
        );
    }

    /// FR-133 P2e: what network sends a caller, inline in the tunnel, SSH and
    /// DERP handlers, so read from the source (docs anchor these files'
    /// lines). The tunnel client prints an open error as `{code}: {message}`
    /// and the tunnel audit stores a forward's reject reason, so each names
    /// the device, never an "agent"; the codes are wire names and stay. The
    /// daemon's supervisor reads an open error by its code (`cross_tenant`),
    /// and a forward's reason must never read as the client's session-death
    /// signal (`is_session_gone_reject`). `roomler ssh` and the web app print
    /// the SSH route's refusals.
    #[test]
    fn network_refusals_say_device_not_agent() {
        use roomler_ai_remote_control::signaling::{
            REJECT_REASON_NO_SESSION, REJECT_REASON_SESSION_GONE, REJECT_REASON_SESSION_MISMATCH,
        };
        fn code(src: &str) -> &str {
            &src[..src.find("#[cfg(test)]").unwrap_or(src.len())]
        }
        let files: [(&str, &str, &[&str], &[&str]); 3] = [
            (
                "tunnel.rs",
                code(include_str!("tunnel.rs")),
                &[
                    "\"device {agent_id} does not exist\"",
                    "\"device belongs to a different tenant\"",
                    "\"device is quarantined or deleted\"",
                    "\"device is homed on another pod; re-dial and retry\"",
                    "\"originating device is quarantined or deleted\"",
                    "\"device row vanished\"",
                    "\"device unreachable: {e}\"",
                ],
                &[
                    "agent {agent_id} does not exist",
                    "agent belongs to a different tenant",
                    "\"agent is quarantined or deleted",
                    "\"agent is homed on another pod",
                    "originating agent is",
                    "agent row vanished",
                    "agent unreachable",
                ],
            ),
            (
                "routes/agent_ssh.rs",
                code(include_str!("routes/agent_ssh.rs")),
                &[
                    "\"Invalid device id\"",
                    "\"Device not found\"",
                    "this device's daemon cannot run SSH sessions as the person at its screen",
                    "this device's daemon does not honour consent_mode yet (needs the P5d daemon)",
                ],
                &[
                    "Invalid agent id",
                    "Agent not found",
                    "this device's agent",
                    "the P5d agent",
                ],
            ),
            (
                "derp.rs",
                code(include_str!("derp.rs")),
                &["\"Invalid device ID\""],
                &["Invalid agent ID"],
            ),
        ];
        for (file, code, says, said) in files {
            for words in says {
                assert!(code.contains(words), "{file} no longer says {words}");
                for phrase in [
                    REJECT_REASON_SESSION_GONE,
                    REJECT_REASON_NO_SESSION,
                    REJECT_REASON_SESSION_MISMATCH,
                ] {
                    assert!(!words.contains(phrase), "{words} reads as session death");
                }
            }
            for words in said {
                assert!(!code.contains(words), "{file} says {words} again");
            }
        }
    }
}

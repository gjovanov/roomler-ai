//! WebSocket handler for `roomler-tunnel` clients (`role=tunnel-client`).
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
    signaling::{ClientMsg, CloseReason, RejectKind, ServerMsg},
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};
use tunnel_core::policy::{
    GateResult, Principal, ProtocolKind, ResolvedSubject, check_forward_request,
};
use tunnel_core::transport::{TRANSPORT_QUIC_V1, TRANSPORT_WEBRTC_DC_V1};

use crate::state::AppState;
use crate::ws::remote_control::pump_server_messages;

/// Outbound channel capacity. Per-tunnel-client. Generous because a
/// single peer multiplexes many flows; the dominant traffic is per-
/// flow audit relays + ICE trickle, both small and sporadic.
const OUTBOUND_CAP: usize = 256;

/// Cadence of the revocation re-check. 60 s is the v1 default —
/// cheap (one Mongo find_by_id per minute per active connection)
/// and matches the existing agent heartbeat rhythm.
const REVOCATION_CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// Drive a tunnel-client WS until either the client closes, the
/// signalling layer errors, or the revocation re-check kills the
/// connection. Replaces the T1 stub message loop with real
/// `rc:tunnel.*` dispatch.
pub async fn handle_tunnel_client_socket(
    state: AppState,
    socket: WebSocket,
    tunnel_client_id: ObjectId,
    tenant_id: ObjectId,
    owner_user_id: ObjectId,
) {
    info!(%tunnel_client_id, %tenant_id, %owner_user_id, "tunnel-client WS connected");

    let (socket_tx, mut socket_rx) = socket.split();
    let socket_tx = Arc::new(Mutex::new(socket_tx));

    // Outbound channel pattern — mirrors the agent WS handler's
    // Hub-owned mpsc. All `ServerMsg` writes go through `outbound_tx`;
    // a single pump task drains the receiver onto the socket. Once
    // the tunnel-session is established we register a clone of
    // `outbound_tx` in `AppState::tunnel_clients_by_session` so the
    // agent's WS handler can push relayed `TcpForwardAccept` etc.
    // back to this client.
    let (outbound_tx, outbound_rx) = mpsc::channel::<ServerMsg>(OUTBOUND_CAP);
    let pump = tokio::spawn(pump_server_messages(outbound_rx, socket_tx.clone()));

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
    let (client_version, client_os) = match state
        .tunnel_clients
        .find_in_tenant(tenant_id, tunnel_client_id)
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
    };

    // Periodic revocation re-check task — same as T1 stub, but now
    // sends a typed `TunnelRevoked` `ServerMsg` instead of an
    // ad-hoc JSON frame.
    let revocation_handle = spawn_revocation_check(
        state.clone(),
        socket_tx.clone(),
        outbound_tx.clone(),
        tunnel_client_id,
        tenant_id,
    );

    // Main read loop.
    while let Some(msg) = socket_rx.next().await {
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
        let Some(parsed) = crate::ws::overlay::relay_overlay_msg_from_node(
            &state,
            crate::ws::overlay::NodeIdentity::TunnelClient(tunnel_client_id),
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
            } => {
                // The dedicated CLI is single-open, so `open_nonce` is
                // normally None; the server echoes back whatever it sent.
                session = handle_tunnel_open(
                    &state,
                    &orig,
                    agent_id,
                    transport,
                    open_nonce,
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
                    let _ = state.rc_hub.send_to_agent(
                        s.agent_id,
                        ServerMsg::TunnelTerminate {
                            session_id: s.tunnel_session_id,
                            reason,
                        },
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
            } => {
                // Relay flow-close to agent + append audit row.
                relay_tcp_closed_to_agent(
                    &state,
                    &orig,
                    session.as_ref(),
                    session_id,
                    flow_id,
                    reason,
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

    revocation_handle.abort();

    // Overlay teardown: drop this node from the registry + mark it
    // offline so peers' netmaps lose it. Best-effort; no-op if the
    // client never joined the overlay.
    state.overlay_nodes_by_id.remove(&tunnel_client_id);
    crate::ws::overlay::handle_overlay_leave(
        &state,
        crate::ws::overlay::NodeIdentity::TunnelClient(tunnel_client_id),
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
        let _ = state.rc_hub.send_to_agent(
            s.agent_id,
            ServerMsg::TunnelTerminate {
                session_id: s.tunnel_session_id,
                reason: CloseReason::ClientShutdown,
            },
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
pub(crate) struct TunnelSession {
    pub(crate) tunnel_session_id: ObjectId,
    agent_id: ObjectId,
    agent_tenant_id: ObjectId,
    #[allow(dead_code)] // T2.6 will plumb this into the agent-WS relay
    transport: String,
}

/// Identity + reply channel of whoever originated a tunnel-CLIENT session
/// on this server. Two principals drive the identical handlers:
///   * `Principal::TunnelClient` — a dedicated `roomler-tunnel` WS
///     (`handle_tunnel_client_socket`).
///   * `Principal::Agent` — an enrolled agent driving the tunnel-client
///     role over its own agent WS (P3b-2,
///     `relay_tunnel_client_msg_from_agent`).
///
/// Bundling the per-originator values here lets ONE subject-agnostic set
/// of handlers serve both WS roles instead of duplicating the open /
/// forward / relay / audit logic per role.
#[derive(Clone)]
pub(crate) struct Originator {
    pub(crate) principal: Principal,
    /// The originator's own tenant. For an agent origin this is the
    /// agent's tenant — the value the cross-tenant gate compares against
    /// the TARGET agent's tenant.
    pub(crate) tenant_id: ObjectId,
    pub(crate) owner_user_id: ObjectId,
    pub(crate) client_version: String,
    pub(crate) client_os: OsKind,
    /// The originator's outbound `ServerMsg` channel. For a tunnel-client
    /// WS this is its per-connection `outbound_tx`; for an agent it is a
    /// clone of the agent's Hub-owned `registered_tx`, so a target's
    /// answers relayed via `tunnel_clients_by_session[session_id]` reach
    /// the agent's socket through its EXISTING pump — no target-side change.
    pub(crate) outbound_tx: mpsc::Sender<ServerMsg>,
}

impl Originator {
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
    state: &AppState,
    orig: &Originator,
    agent_id: ObjectId,
    transport: String,
    open_nonce: Option<String>,
    client_supported: &[String],
) -> Option<TunnelSession> {
    // 1. Fetch the agent (any tenant — we need the row to enforce
    // the cross-tenant gate ourselves). `find_in_tenant` is wrong
    // here because it scopes by tenant — we need the agent's actual
    // tenant_id to compare. Use a direct lookup via the base DAO.
    let agent = match state.agents.base.find_by_id(agent_id).await {
        Ok(a) => a,
        Err(_) => {
            send_msg(
                &orig.outbound_tx,
                ServerMsg::Error {
                    session_id: None,
                    code: "agent_not_found".into(),
                    message: format!("agent {agent_id} does not exist"),
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
                message: "agent belongs to a different tenant".into(),
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
                message: "agent is quarantined or deleted".into(),
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
            state.agents.base.find_by_id(origin_id).await,
            Ok(a) if a.deleted_at.is_none() && !matches!(a.status, AgentStatus::Quarantined)
        );
        if !origin_ok {
            send_msg(
                &orig.outbound_tx,
                ServerMsg::Error {
                    session_id: None,
                    code: "origin_unavailable".into(),
                    message: "originating agent is quarantined or deleted".into(),
                    open_nonce: open_nonce.clone(),
                },
            )
            .await;
            return None;
        }
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
    // otherwise).
    let agent_speaks_quic = tunnel_core::transport::agent_supports_quic(&agent.agent_version);
    let negotiated_transport = if transport == TRANSPORT_QUIC_V1 {
        if client_supported.iter().any(|t| t == TRANSPORT_QUIC_V1) && agent_speaks_quic {
            TRANSPORT_QUIC_V1.to_string()
        } else {
            debug!(
                %agent_id, agent_version = %agent.agent_version, agent_speaks_quic,
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
    let quic_auth_token = if negotiated_transport == TRANSPORT_QUIC_V1 {
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
    let quic_ice_servers = roomler_ai_remote_control::turn_creds::ice_servers_for(
        &tunnel_session_id.to_hex(),
        crate::state::build_turn_config(&state.settings.turn).as_ref(),
    );

    // For quic-v1, tell the agent to stand up its endpoint + authorize
    // this token NOW. The client is already registered in
    // `tunnel_clients_by_session` (step 4), so the agent's
    // `TunnelQuicReady` can race back and be relayed to the client
    // while it's still processing the `TunnelOpened` below.
    if let Some(token) = quic_auth_token.clone()
        && let Err(e) = state.rc_hub.send_to_agent(
            agent_id,
            ServerMsg::TunnelQuicSetup {
                session_id: tunnel_session_id,
                quic_auth_token: token,
                ice_servers: quic_ice_servers.clone(),
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

/// Server-side forward gate + relay, shared by TCP CONNECT
/// (`proto = Tcp`) and UDP ASSOCIATE (`proto = Udp`) forwards. The ACL
/// eval, cross-tenant gate, agent-liveness re-check, and audit trail are
/// identical; `proto` selects the wire discriminator + the policy
/// protocol axis.
#[allow(clippy::too_many_arguments)]
async fn handle_forward_request(
    proto: ProtocolKind,
    state: &AppState,
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
                "no open session (send rc:tunnel.open first)".into(),
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
                "session_id mismatch".into(),
            ),
        )
        .await;
        return;
    }

    // Re-fetch the agent row each time so a quarantine that lands
    // mid-session bites the next forward.
    let agent = match state.agents.base.find_by_id(s.agent_id).await {
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
                "agent row vanished",
            )
            .await;
            send_msg(
                &orig.outbound_tx,
                forward_reject_msg(
                    proto,
                    request_session_id,
                    flow_id,
                    RejectKind::AgentError,
                    "agent row vanished".into(),
                ),
            )
            .await;
            return;
        }
    };

    // Active policies for the agent's tenant. Server-side ACL is
    // the auth boundary; the agent runs its own minimal allowlist
    // as defence-in-depth (T2.6).
    let policies = match state
        .tunnel_policies
        .list_active_for_tenant(s.agent_tenant_id)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            warn!(origin = %orig.log_id(), %e, "policy fetch failed; defaulting to deny");
            Vec::new()
        }
    };

    let subject = ResolvedSubject {
        user_id: orig.owner_user_id,
        // T2.6 will resolve role_ids via the existing tenant
        // membership lookup. For T2.5 we use an empty list — only
        // UserId / TunnelClientId / AgentId / AllUsers policy subjects
        // match.
        role_ids: Vec::new(),
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
            let relay = state.rc_hub.send_to_agent(
                s.agent_id,
                forward_forward_msg(
                    proto,
                    request_session_id,
                    flow_id,
                    dst_host.to_string(),
                    dst_port,
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
                    audit_tcp_reject(
                        state,
                        s,
                        orig,
                        flow_id,
                        dst_host,
                        dst_port,
                        RejectKind::AgentError,
                        &format!("agent unreachable: {e}"),
                    )
                    .await;
                    send_msg(
                        &orig.outbound_tx,
                        forward_reject_msg(
                            proto,
                            request_session_id,
                            flow_id,
                            RejectKind::AgentError,
                            format!("agent unreachable: {e}"),
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
    state: &AppState,
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
    state: &AppState,
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

async fn audit_peer_close(state: &AppState, session: &TunnelSession, orig: &Originator) {
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
    state: &AppState,
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
    if let Err(e) = state.rc_hub.send_to_agent(
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
    state: &AppState,
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
    if let Err(e) = state.rc_hub.send_to_agent(
        s.agent_id,
        ServerMsg::TcpClosed {
            session_id: request_session_id,
            flow_id,
            reason,
        },
    ) {
        debug!(origin = %orig.log_id(), %flow_id, %e, "tcp-closed relay to agent failed");
    }
    audit_tcp_close(state, s, orig, flow_id, reason).await;
}

/// UDP analogue of [`relay_tcp_closed_to_agent`]: relay a UDP-flow close
/// to the agent + append an audit row. Reuses [`audit_tcp_close`] — the
/// close accounting is L4-agnostic (flow_id + reason).
async fn relay_udp_closed_to_agent(
    state: &AppState,
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
    if let Err(e) = state.rc_hub.send_to_agent(
        s.agent_id,
        ServerMsg::UdpClosed {
            session_id: request_session_id,
            flow_id,
            reason,
        },
    ) {
        debug!(origin = %orig.log_id(), %flow_id, %e, "udp-closed relay to agent failed");
    }
    audit_tcp_close(state, s, orig, flow_id, reason).await;
}

/// Relay a tunnel-client SDP offer to the agent. Cheap session_id
/// validation only — the heavy gates (cross-tenant, agent online)
/// already fired on TunnelOpen, so reaching here means the session
/// is sound.
async fn relay_sdp_offer_to_agent(
    state: &AppState,
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
    if let Err(e) = state.rc_hub.send_to_agent(
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
    state: &AppState,
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
    if let Err(e) = state.rc_hub.send_to_agent(
        s.agent_id,
        ServerMsg::TunnelIce {
            session_id: request_session_id,
            candidate,
        },
    ) {
        debug!(%request_session_id, %e, "tunnel ICE relay to agent failed");
    }
}

/// Relay the tunnel-client's QUIC relay candidate(s) to the agent so it
/// can install a TURN permission for the client's relay address before
/// the QUIC handshake (Phase 3c). Mirror of [`relay_ice_to_agent`].
async fn relay_quic_candidate_to_agent(
    state: &AppState,
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
    if let Err(e) = state.rc_hub.send_to_agent(
        s.agent_id,
        ServerMsg::TunnelQuicCandidate {
            session_id: request_session_id,
            addrs,
        },
    ) {
        debug!(%request_session_id, %e, "tunnel QUIC candidate relay to agent failed");
    }
}

async fn audit_tcp_close(
    state: &AppState,
    session: &TunnelSession,
    orig: &Originator,
    flow_id: u32,
    reason: CloseReason,
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
            bytes_in: 0,
            bytes_out: 0,
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

fn spawn_revocation_check(
    state: AppState,
    socket_tx: Arc<Mutex<SplitSink<WebSocket, Message>>>,
    outbound_tx: mpsc::Sender<ServerMsg>,
    tunnel_client_id: ObjectId,
    tenant_id: ObjectId,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(REVOCATION_CHECK_INTERVAL);
        tick.tick().await;
        loop {
            tick.tick().await;
            match state
                .tunnel_clients
                .find_in_tenant(tenant_id, tunnel_client_id)
                .await
            {
                Ok(c)
                    if c.deleted_at.is_none()
                        && matches!(c.status, AgentStatus::Online | AgentStatus::Offline) =>
                {
                    let _ = state.tunnel_clients.touch_heartbeat(tunnel_client_id).await;
                }
                Ok(_) => {
                    info!(%tunnel_client_id, "tunnel-client revoked mid-session; closing WS");
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
                }
            }
        }
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// P3b-2: an agent driving the tunnel-CLIENT role over its own agent WS
// ─────────────────────────────────────────────────────────────────────────────

/// Agent-WS interception for the tunnel-CLIENT role (P3b-2, identity model
/// (b)). An enrolled agent may ORIGINATE tunnel-client sessions over its
/// own agent WS. This runs FIRST in the agent read-loop — ahead of the
/// target-side [`crate::ws::remote_control::relay_tunnel_msg_from_agent`] —
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
pub(crate) async fn relay_tunnel_client_msg_from_agent(
    state: &AppState,
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
        } => {
            if let Some(s) = handle_tunnel_open(
                state,
                orig,
                agent_id,
                transport,
                open_nonce,
                supported_transports,
            )
            .await
            {
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
        } => {
            if sessions.contains_key(&session_id) {
                relay_tcp_closed_to_agent(
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
                Some(ClientMsg::TcpClosed {
                    session_id,
                    flow_id,
                    reason,
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
                let _ = state.rc_hub.send_to_agent(
                    s.agent_id,
                    ServerMsg::TunnelTerminate {
                        session_id: s.tunnel_session_id,
                        reason,
                    },
                );
                state.tunnel_clients_by_session.remove(&s.tunnel_session_id);
                unindex_session_target(
                    &state.tunnel_sessions_by_target_agent,
                    s.agent_id,
                    s.tunnel_session_id,
                );
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
pub(crate) async fn teardown_agent_originated_sessions(
    state: &AppState,
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
        let _ = state.rc_hub.send_to_agent(
            s.agent_id,
            ServerMsg::TunnelTerminate {
                session_id: s.tunnel_session_id,
                reason: CloseReason::ClientShutdown,
            },
        );
        audit_peer_close(state, &s, orig).await;
    }
}

/// Drop one session from the by-target-agent index (see
/// `AppState::tunnel_sessions_by_target_agent`), tidying away the agent's
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
pub(crate) async fn terminate_sessions_targeting_agent(state: &AppState, agent_id: ObjectId) {
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
            send_msg(
                &tx,
                ServerMsg::TunnelTerminate {
                    session_id: sid,
                    reason: CloseReason::ServerTerminated,
                },
            )
            .await;
        }
    }
}

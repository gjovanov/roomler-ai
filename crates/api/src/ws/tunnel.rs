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
    models::{AgentStatus, RelayMode, TunnelAuditEvent, TunnelAuditKind},
    signaling::{ClientMsg, RejectKind, ServerMsg},
};
use roomler_ai_tunnel_core::policy::{GateResult, ResolvedSubject, check_forward_request};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::state::AppState;

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

    // Periodic revocation re-check task — same as T1 stub, but now
    // sends a typed `TunnelRevoked` `ServerMsg` instead of an
    // ad-hoc JSON frame.
    let revocation_handle = spawn_revocation_check(
        state.clone(),
        socket_tx.clone(),
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
                    &socket_tx,
                    None,
                    "bad_request",
                    &format!("malformed JSON: {e}"),
                )
                .await;
                continue;
            }
        };

        match parsed {
            ClientMsg::TunnelHello {
                role: _,
                version,
                supported_transports,
            } => {
                debug!(%tunnel_client_id, %version, ?supported_transports, "rc:tunnel.hello");
                // T2.5 stub: nothing to negotiate yet — v1 ships
                // only "webrtc-dc-v1". The transport selection lands
                // on `TunnelOpen` where the server replies with the
                // chosen transport in `TunnelOpened`.
            }

            ClientMsg::TunnelOpen {
                agent_id,
                transport,
            } => {
                handle_tunnel_open(
                    &state,
                    &socket_tx,
                    &mut session,
                    tunnel_client_id,
                    tenant_id,
                    owner_user_id,
                    &client_version,
                    client_os,
                    agent_id,
                    transport,
                )
                .await;
            }

            ClientMsg::TcpForwardRequest {
                session_id,
                flow_id,
                dst_host,
                dst_port,
            } => {
                handle_tcp_forward_request(
                    &state,
                    &socket_tx,
                    session.as_ref(),
                    tunnel_client_id,
                    tenant_id,
                    owner_user_id,
                    &client_version,
                    client_os,
                    session_id,
                    flow_id,
                    &dst_host,
                    dst_port,
                )
                .await;
            }

            // T2.6+ — these need the agent WS bridge.
            ClientMsg::TunnelTerminate { session_id, reason } => {
                info!(%tunnel_client_id, %session_id, ?reason, "rc:tunnel.terminate");
                if let Some(s) = session.take() {
                    audit_peer_close(
                        &state,
                        &s,
                        owner_user_id,
                        tunnel_client_id,
                        &client_version,
                        client_os,
                    )
                    .await;
                }
            }
            ClientMsg::TcpHalfClose { .. }
            | ClientMsg::TcpClosed { .. }
            | ClientMsg::TcpForwardAccept { .. }
            | ClientMsg::TcpForwardReject { .. } => {
                // T2.6 will relay these to the agent's WS. For now,
                // log so we can see the wire is exercised by clients.
                debug!(%tunnel_client_id, ?parsed, "rc:tunnel.* relay variant — T2.6");
            }

            // Non-tunnel rc:* — explicitly ignored on this WS role.
            other => {
                debug!(%tunnel_client_id, ?other, "non-tunnel rc:* on tunnel-client WS — ignored");
            }
        }
    }

    revocation_handle.abort();
    if let Some(s) = session {
        audit_peer_close(
            &state,
            &s,
            owner_user_id,
            tunnel_client_id,
            &client_version,
            client_os,
        )
        .await;
    }
    info!(%tunnel_client_id, "tunnel-client WS disconnected");
}

/// Per-connection state created on `TunnelOpen` and consumed by every
/// subsequent flow event for audit correlation.
#[derive(Debug, Clone)]
struct TunnelSession {
    tunnel_session_id: ObjectId,
    agent_id: ObjectId,
    agent_tenant_id: ObjectId,
    #[allow(dead_code)] // T2.6 will plumb this into the agent-WS relay
    transport: String,
}

#[allow(clippy::too_many_arguments)]
async fn handle_tunnel_open(
    state: &AppState,
    socket_tx: &Arc<Mutex<SplitSink<WebSocket, Message>>>,
    session: &mut Option<TunnelSession>,
    tunnel_client_id: ObjectId,
    client_tenant_id: ObjectId,
    owner_user_id: ObjectId,
    client_version: &str,
    client_os: roomler_ai_remote_control::models::OsKind,
    agent_id: ObjectId,
    transport: String,
) {
    // 1. Fetch the agent (any tenant — we need the row to enforce
    // the cross-tenant gate ourselves). `find_in_tenant` is wrong
    // here because it scopes by tenant — we need the agent's actual
    // tenant_id to compare. Use a direct lookup via the base DAO.
    let agent = match state.agents.base.find_by_id(agent_id).await {
        Ok(a) => a,
        Err(_) => {
            send(
                socket_tx,
                &ServerMsg::Error {
                    session_id: None,
                    code: "agent_not_found".into(),
                    message: format!("agent {agent_id} does not exist"),
                },
            )
            .await;
            return;
        }
    };

    // 2. Cross-tenant gate (Sev0 — see plan §"Multi-tenancy gotcha").
    if agent.tenant_id != client_tenant_id {
        warn!(
            %tunnel_client_id, %agent_id, %client_tenant_id,
            agent_tenant_id = %agent.tenant_id,
            "tunnel-client tried to open peer to a cross-tenant agent"
        );
        send(
            socket_tx,
            &ServerMsg::Error {
                session_id: None,
                code: "cross_tenant".into(),
                message: "agent belongs to a different tenant".into(),
            },
        )
        .await;
        return;
    }

    // 3. Refuse if agent is soft-deleted or quarantined — early
    // signal beats waiting for the relay step to fail.
    if agent.deleted_at.is_some() || matches!(agent.status, AgentStatus::Quarantined) {
        send(
            socket_tx,
            &ServerMsg::Error {
                session_id: None,
                code: "agent_unavailable".into(),
                message: "agent is quarantined or deleted".into(),
            },
        )
        .await;
        return;
    }

    // 4. Create the session id + persist on the connection.
    let tunnel_session_id = ObjectId::new();
    let new_session = TunnelSession {
        tunnel_session_id,
        agent_id,
        agent_tenant_id: agent.tenant_id,
        transport: transport.clone(),
    };
    *session = Some(new_session.clone());

    // 5. Audit the open. RelayMode is "Direct" until ICE finishes —
    // T2.7 updates this after candidate selection.
    let _ = state
        .tunnel_audit
        .append(&TunnelAuditEvent {
            id: None,
            tenant_id: client_tenant_id,
            tunnel_session_id,
            tunnel_client_id,
            agent_id,
            user_id: owner_user_id,
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
            client_version: client_version.to_string(),
            client_os,
            reason: None,
        })
        .await;

    // 6. Reply with TunnelOpened. ICE candidates + actual SDP come in
    // T2.7 once the WebRTC peer negotiation lands. For now we send
    // empty ice_servers; the SCTP rwnd value mirrors the vendored
    // webrtc patch's target so the CLI's `diagnose` subcommand can
    // verify the patch took effect.
    send(
        socket_tx,
        &ServerMsg::TunnelOpened {
            session_id: tunnel_session_id,
            transport,
            dc_pool_size: 8,
            sctp_rwnd_bytes: 8 * 1024 * 1024,
            ice_servers: vec![],
        },
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn handle_tcp_forward_request(
    state: &AppState,
    socket_tx: &Arc<Mutex<SplitSink<WebSocket, Message>>>,
    session: Option<&TunnelSession>,
    tunnel_client_id: ObjectId,
    client_tenant_id: ObjectId,
    owner_user_id: ObjectId,
    client_version: &str,
    client_os: roomler_ai_remote_control::models::OsKind,
    request_session_id: ObjectId,
    flow_id: u32,
    dst_host: &str,
    dst_port: u16,
) {
    let Some(s) = session else {
        // No prior TunnelOpen — client is using the wire wrong.
        send(
            socket_tx,
            &ServerMsg::TcpForwardReject {
                session_id: request_session_id,
                flow_id,
                kind: RejectKind::AgentError,
                reason: "no open session (send rc:tunnel.open first)".into(),
            },
        )
        .await;
        return;
    };
    if s.tunnel_session_id != request_session_id {
        send(
            socket_tx,
            &ServerMsg::TcpForwardReject {
                session_id: request_session_id,
                flow_id,
                kind: RejectKind::AgentError,
                reason: "session_id mismatch".into(),
            },
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
                tunnel_client_id,
                owner_user_id,
                client_version,
                client_os,
                flow_id,
                dst_host,
                dst_port,
                RejectKind::AgentError,
                "agent row vanished",
            )
            .await;
            send(
                socket_tx,
                &ServerMsg::TcpForwardReject {
                    session_id: request_session_id,
                    flow_id,
                    kind: RejectKind::AgentError,
                    reason: "agent row vanished".into(),
                },
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
            warn!(%tunnel_client_id, %e, "policy fetch failed; defaulting to deny");
            Vec::new()
        }
    };

    let subject = ResolvedSubject {
        user_id: owner_user_id,
        // T2.6 will resolve role_ids via the existing tenant
        // membership lookup. For T2.5 we use an empty list — only
        // UserId / TunnelClientId / AllUsers policy subjects match.
        role_ids: Vec::new(),
        tunnel_client_id,
    };

    let result = check_forward_request(
        client_tenant_id,
        &agent,
        &policies,
        &subject,
        dst_host,
        dst_port,
    );

    match result {
        GateResult::Allow { policy_id, .. } => {
            debug!(%tunnel_client_id, %flow_id, %dst_host, %dst_port, %policy_id, "tcp forward allowed");
            // T2.5 stub: tell the client the forward is accepted with
            // dc_index 0. T2.6 will actually relay the
            // `TcpForwardForward` to the agent + wait for its real
            // accept reply.
            audit_tcp_accept(
                state,
                s,
                tunnel_client_id,
                owner_user_id,
                client_version,
                client_os,
                flow_id,
                dst_host,
                dst_port,
            )
            .await;
            send(
                socket_tx,
                &ServerMsg::TcpForwardAccept {
                    session_id: request_session_id,
                    flow_id,
                    dc_index: 0,
                },
            )
            .await;
        }
        GateResult::Reject { kind, reason } => {
            info!(%tunnel_client_id, %flow_id, %dst_host, %dst_port, ?kind, %reason, "tcp forward rejected");
            audit_tcp_reject(
                state,
                s,
                tunnel_client_id,
                owner_user_id,
                client_version,
                client_os,
                flow_id,
                dst_host,
                dst_port,
                kind,
                &reason,
            )
            .await;
            send(
                socket_tx,
                &ServerMsg::TcpForwardReject {
                    session_id: request_session_id,
                    flow_id,
                    kind,
                    reason,
                },
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
    tunnel_client_id: ObjectId,
    owner_user_id: ObjectId,
    client_version: &str,
    client_os: roomler_ai_remote_control::models::OsKind,
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
            tunnel_client_id,
            agent_id: session.agent_id,
            user_id: owner_user_id,
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
            client_version: client_version.to_string(),
            client_os,
            reason: None,
        })
        .await;
}

#[allow(clippy::too_many_arguments)]
async fn audit_tcp_reject(
    state: &AppState,
    session: &TunnelSession,
    tunnel_client_id: ObjectId,
    owner_user_id: ObjectId,
    client_version: &str,
    client_os: roomler_ai_remote_control::models::OsKind,
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
            tunnel_client_id,
            agent_id: session.agent_id,
            user_id: owner_user_id,
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
            client_version: client_version.to_string(),
            client_os,
            reason: Some(format!("{kind:?}: {reason}")),
        })
        .await;
}

async fn audit_peer_close(
    state: &AppState,
    session: &TunnelSession,
    owner_user_id: ObjectId,
    tunnel_client_id: ObjectId,
    client_version: &str,
    client_os: roomler_ai_remote_control::models::OsKind,
) {
    let _ = state
        .tunnel_audit
        .append(&TunnelAuditEvent {
            id: None,
            tenant_id: session.agent_tenant_id,
            tunnel_session_id: session.tunnel_session_id,
            tunnel_client_id,
            agent_id: session.agent_id,
            user_id: owner_user_id,
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
            client_version: client_version.to_string(),
            client_os,
            reason: None,
        })
        .await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Wire helpers
// ─────────────────────────────────────────────────────────────────────────────

async fn send(socket_tx: &Arc<Mutex<SplitSink<WebSocket, Message>>>, msg: &ServerMsg) {
    let text = match serde_json::to_string(msg) {
        Ok(t) => t,
        Err(e) => {
            warn!(%e, "ServerMsg serialise failed; dropping");
            return;
        }
    };
    let mut guard = socket_tx.lock().await;
    let _ = guard.send(Message::text(text)).await;
}

async fn send_error(
    socket_tx: &Arc<Mutex<SplitSink<WebSocket, Message>>>,
    session_id: Option<ObjectId>,
    code: &str,
    message: &str,
) {
    send(
        socket_tx,
        &ServerMsg::Error {
            session_id,
            code: code.into(),
            message: message.into(),
        },
    )
    .await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Revocation re-check (lifted from T1 stub, now sends typed ServerMsg)
// ─────────────────────────────────────────────────────────────────────────────

fn spawn_revocation_check(
    state: AppState,
    socket_tx: Arc<Mutex<SplitSink<WebSocket, Message>>>,
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
                    send(
                        &socket_tx,
                        &ServerMsg::TunnelRevoked {
                            reason: "status changed to Quarantined or soft-deleted".into(),
                        },
                    )
                    .await;
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

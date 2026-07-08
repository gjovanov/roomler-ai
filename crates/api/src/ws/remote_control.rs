//! WebSocket glue for the remote-control subsystem.
//!
//! The `roomler-ai-remote-control` crate owns the state machine and the
//! registry of agents/controllers ([`Hub`]). This module is the thin bridge
//! between an Axum [`WebSocket`] and the Hub: it pumps [`ServerMsg`] values
//! from a per-connection [`mpsc::Receiver`] out to the socket, parses inbound
//! [`ClientMsg`] values and forwards them to [`Hub::dispatch`].

use axum::extract::ws::{Message, WebSocket};
use bson::oid::ObjectId;
use futures::{SinkExt, StreamExt, stream::SplitSink};
use roomler_ai_remote_control::{
    Hub,
    hub::DispatchCtx,
    models::ConsentMode,
    signaling::{ClientMsg, Role, ServerMsg},
};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};

use crate::state::AppState;

/// Handle a socket that authenticated as an agent.
///
/// Lifecycle: verify + look up agent, expect `rc:agent.hello`, register with
/// the Hub, then relay `rc:*` traffic in both directions until the socket closes.
pub async fn handle_agent_socket(
    state: AppState,
    socket: WebSocket,
    agent_id: ObjectId,
    tenant_id: ObjectId,
    owner_user_id: ObjectId,
) {
    info!(%agent_id, %tenant_id, "remote-control agent WS connected");

    let (socket_tx, mut socket_rx) = socket.split();
    let socket_tx = Arc::new(Mutex::new(socket_tx));

    // Wait for the agent's hello message — it announces OS + capabilities.
    let hello = match read_next_rc(&mut socket_rx).await {
        Some(ClientMsg::AgentHello {
            machine_name,
            os,
            agent_version,
            displays,
            caps,
        }) => (machine_name, os, agent_version, displays, caps),
        other => {
            warn!(?other, "agent opened WS without rc:agent.hello — closing");
            return;
        }
    };
    let (machine_name, os, agent_version, displays, caps) = hello;

    // Persist: mark online, update hello fields on the Mongo row. Best-effort —
    // signaling still works if Mongo lags.
    if let Err(e) = state
        .agents
        .update_hello(agent_id, &agent_version, &displays, &caps)
        .await
    {
        warn!(%agent_id, %e, "agent update_hello failed");
    }

    // Register with the Hub and start pumping server → socket.
    //
    // rc.53: register_agent now returns `(tx, cancel, rx)`:
    //   * `tx` is captured for the eventual `unregister_agent` call so
    //     a displaced-handler late unregister doesn't evict a newer
    //     connection's entry (critique #4 race fix).
    //   * `cancel` is an `Arc<Notify>` the read-loop `select!`s on,
    //     so a displacement triggers an immediate read-loop exit
    //     instead of waiting up to one 25 s keepalive interval.
    //   * `rx` feeds the pump task as before.
    let max_sessions = caps.max_simultaneous_sessions.max(1);
    let (registered_tx, cancel, rx) =
        state
            .rc_hub
            .register_agent(agent_id, tenant_id, owner_user_id, os, max_sessions);
    let pump_socket_tx = socket_tx.clone();
    let pump = tokio::spawn(pump_server_messages(rx, pump_socket_tx));

    debug!(%agent_id, %machine_name, "agent registered in Hub");

    // Build a ctx once — it's Copy-able across messages for this connection.
    let ctx = DispatchCtx {
        role: Role::Agent,
        user_id: None,
        agent_id: Some(agent_id),
        controller_name: None,
        controller_tx: None,
        // Unused for agent-role dispatch (only a controller's SessionRequest
        // consumes these); harmless defaults.
        consent_mode: ConsentMode::Prompt,
        override_reason: None,
    };

    // Read loop. rc.53: wrapped in `tokio::select!` so the Hub's
    // displacement-cancel notify exits this loop within milliseconds
    // — without the cancel arm, a displaced socket would linger up
    // to one 25 s keepalive interval (auto-fail #3 in v2 plan).
    loop {
        tokio::select! {
            // `biased` so cancel fires deterministically when both
            // arms are ready in the same poll cycle. Without this,
            // tokio's random arm selection could starve the cancel
            // for several iterations in a hot read loop.
            biased;
            _ = cancel.notified() => {
                info!(%agent_id, "agent connection cancelled by Hub (replaced by newer); exiting read-loop");
                break;
            }
            maybe_msg = socket_rx.next() => {
                let Some(msg) = maybe_msg else { break };
                match msg {
                    Ok(Message::Text(text)) => match serde_json::from_str::<ClientMsg>(&text) {
                        Ok(parsed) => {
                            // Tunnel-flow variants intercept first — Hub doesn't
                            // know about tunnel-clients, so we route directly
                            // through `AppState::tunnel_clients_by_session`. If
                            // it's not a tunnel-flow variant the helper returns
                            // the value unchanged for the Hub to handle.
                            let Some(parsed) = relay_tunnel_msg_from_agent(&state, parsed).await else {
                                continue;
                            };
                            // Overlay `rc:overlay.*` variants are brokered
                            // here too (the Hub doesn't know about the
                            // overlay). Consumed messages return None.
                            let Some(parsed) = crate::ws::overlay::relay_overlay_msg_from_node(
                                &state,
                                crate::ws::overlay::NodeIdentity::Agent(agent_id),
                                parsed,
                            )
                            .await
                            else {
                                continue;
                            };
                            // Phase 7: refresh last_seen_at on every heartbeat. Hub
                            // dispatch is a no-op for AgentHeartbeat (handled here);
                            // we still call dispatch so any future routing logic
                            // (e.g. metrics fan-out) only needs one entry point.
                            let is_heartbeat = matches!(&parsed, ClientMsg::AgentHeartbeat { .. });
                            if let Err(e) = state.rc_hub.dispatch(&ctx, parsed) {
                                warn!(%agent_id, %e, "rc:* dispatch failed (agent)");
                            }
                            if is_heartbeat && let Err(e) = state.agents.touch_heartbeat(agent_id).await {
                                warn!(%agent_id, %e, "agent touch_heartbeat failed");
                            }
                        }
                        Err(e) => {
                            debug!(%agent_id, %e, "ignoring non-rc:* message on agent socket");
                        }
                    },
                    Ok(Message::Ping(data)) => {
                        let mut guard = socket_tx.lock().await;
                        let _ = guard.send(Message::Pong(data)).await;
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
        }
    }

    // Teardown: unregister + mark offline. rc.53: thread `registered_tx`
    // into unregister_agent so a displaced-handler late unregister
    // doesn't evict the newer connection's registry entry
    // (critique #4 race fix). Pump task exits when the Hub drops its
    // sender (during unregister_agent), so we don't need to abort it
    // explicitly — but the `pump.abort()` is kept as a belt-and-
    // suspenders for the case where the tx-identity check skipped
    // the removal and the pump is still wired to the live channel.
    // If this agent was an overlay node, mark it offline + drop it from
    // peers' netmaps (best-effort; it re-syncs on its next join).
    crate::ws::overlay::handle_overlay_leave(
        &state,
        crate::ws::overlay::NodeIdentity::Agent(agent_id),
    )
    .await;

    state
        .rc_hub
        .unregister_agent(agent_id, Some(&registered_tx));
    pump.abort();
    if let Err(e) = state
        .agents
        .mark_status(
            agent_id,
            roomler_ai_remote_control::models::AgentStatus::Offline,
        )
        .await
    {
        warn!(%agent_id, %e, "agent mark_status(offline) failed");
    }
    info!(%agent_id, "remote-control agent WS disconnected");
}

/// Parse the next inbound WS text frame as [`ClientMsg`]. Skips non-text frames.
async fn read_next_rc(
    socket_rx: &mut futures::stream::SplitStream<WebSocket>,
) -> Option<ClientMsg> {
    while let Some(msg) = socket_rx.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                if let Ok(parsed) = serde_json::from_str::<ClientMsg>(&text) {
                    return Some(parsed);
                }
            }
            Ok(Message::Close(_)) | Err(_) => return None,
            _ => continue,
        }
    }
    None
}

/// Forwards [`ServerMsg`] values from a Hub-owned [`mpsc::Receiver`] to a
/// WebSocket sink. Exits when the channel closes or a send fails.
pub async fn pump_server_messages(
    mut rx: mpsc::Receiver<ServerMsg>,
    socket_tx: Arc<Mutex<SplitSink<WebSocket, Message>>>,
) {
    while let Some(msg) = rx.recv().await {
        let json = match serde_json::to_string(&msg) {
            Ok(s) => s,
            Err(e) => {
                warn!(%e, "serializing ServerMsg failed");
                continue;
            }
        };
        let mut guard = socket_tx.lock().await;
        if guard.send(Message::text(json)).await.is_err() {
            break;
        }
    }
}

/// Route a parsed `rc:*` message coming from a controller browser tab.
/// Returns `true` if the message was handled, `false` if it wasn't rc:*.
pub fn dispatch_controller_rc(
    hub: &Hub,
    user_id: ObjectId,
    controller_name: &str,
    controller_tx: &roomler_ai_remote_control::session::ClientTx,
    text: &str,
    consent_mode: ConsentMode,
    override_reason: Option<String>,
) -> bool {
    let Ok(parsed) = serde_json::from_str::<ClientMsg>(text) else {
        return false;
    };
    let ctx = DispatchCtx {
        role: Role::Controller,
        user_id: Some(user_id),
        agent_id: None,
        controller_name: Some(controller_name.to_string()),
        controller_tx: Some(controller_tx.clone()),
        consent_mode,
        override_reason,
    };
    if let Err(e) = hub.dispatch(&ctx, parsed) {
        warn!(%user_id, %e, "rc:* dispatch failed (controller)");
        // Surface the failure to the controller so the UI can exit its
        // "Requesting session…" spinner instead of hanging. Best-effort —
        // the controller may already be closing.
        let _ = controller_tx.try_send(ServerMsg::Error {
            session_id: error_session_id(&e),
            code: error_code(&e).to_string(),
            message: e.to_string(),
        });
    }
    true
}

/// Result of the session authz gate: the effective consent mode plus a VALIDATED
/// admin break-glass reason (Phase 5). `override_reason` is `Some` only when an
/// `ADMINISTRATOR` force-started a device they don't own with a non-empty reason
/// — in which case `mode` is `Auto` (consent skipped) and the Hub records an
/// `AdminOverride` audit.
pub struct SessionAuthz {
    pub mode: ConsentMode,
    pub override_reason: Option<String>,
}

impl SessionAuthz {
    fn allow(mode: ConsentMode) -> Self {
        Self {
            mode,
            override_reason: None,
        }
    }
}

/// Authorization + consent-mode gate for `rc:session.request` — the Hub can't
/// do this because the `remote_control` crate sits below `services` in the dep
/// graph and has no access to tenant roles. Returns `Ok(SessionAuthz)` for an
/// allowed request, or `Err(reason)` to DENY. A non-`SessionRequest` rc:* message
/// returns `Ok(allow(Prompt))` — those are intra-session and the mode is unused.
///
/// Layers (coarse→fine authz): quarantine → self-control → tenant capability
/// (`ADMINISTRATOR` / `REMOTE_CONTROL`) → per-agent allowlist (empty = no
/// per-device restriction). Consent mode: self-control → `Auto`; else the
/// device's `effective_consent_mode()`. **Break-glass (Phase 5):** an
/// `ADMINISTRATOR` who sends a non-empty `override_reason` for a device they
/// don't own gets `Auto` (consent skipped) + the reason carried through for the
/// `AdminOverride` audit.
pub async fn resolve_session_authz(
    state: &AppState,
    controller_user_id: ObjectId,
    text: &str,
) -> Result<SessionAuthz, String> {
    use roomler_ai_db::models::role::permissions;
    use roomler_ai_remote_control::models::AgentStatus;

    let (agent_id, override_reason) = match serde_json::from_str::<ClientMsg>(text) {
        Ok(ClientMsg::SessionRequest {
            agent_id,
            override_reason,
            ..
        }) => (agent_id, override_reason),
        // Not a session request → the mode is unused; allow through.
        _ => return Ok(SessionAuthz::allow(ConsentMode::Prompt)),
    };

    // Unknown / soft-deleted agent → let the Hub answer with a clean
    // AgentNotFound rather than surfacing a permission error (the mode is moot —
    // create_session will fail on the agent lookup).
    let agent = match state.agents.base.find_by_id(agent_id).await {
        Ok(a) if a.deleted_at.is_none() => a,
        _ => return Ok(SessionAuthz::allow(ConsentMode::Prompt)),
    };

    if agent.status == AgentStatus::Quarantined {
        return Err("device is quarantined; new sessions are blocked".to_string());
    }

    // Controlling your OWN device is always allowed AND auto-consents.
    if agent.owner_user_id == controller_user_id {
        return Ok(SessionAuthz::allow(ConsentMode::Auto));
    }

    // The effective mode for an allowed non-owner controller (attended default).
    let mode = agent.access_policy.effective_consent_mode();

    let perms = state
        .tenants
        .get_member_permissions(agent.tenant_id, controller_user_id)
        .await
        .unwrap_or(0);
    if permissions::has(perms, permissions::ADMINISTRATOR) {
        // Phase 5 break-glass: an ADMINISTRATOR may SKIP consent, but only with a
        // non-empty reason. A blank/absent reason ⇒ the admin gets the device's
        // normal consent mode (no forced override).
        if let Some(reason) = override_reason.filter(|r| !r.trim().is_empty()) {
            return Ok(SessionAuthz {
                mode: ConsentMode::Auto,
                override_reason: Some(reason),
            });
        }
        return Ok(SessionAuthz::allow(mode));
    }
    if !permissions::has(perms, permissions::REMOTE_CONTROL) {
        return Err("you don't have permission to control others' devices".to_string());
    }

    // Per-agent allowlist. Empty ⇒ no per-device restriction (any operator may
    // request; consent is the real gate). Non-empty ⇒ user or a role must match.
    let policy = &agent.access_policy;
    if policy.allowed_user_ids.is_empty() && policy.allowed_role_ids.is_empty() {
        return Ok(SessionAuthz::allow(mode));
    }
    if policy.allowed_user_ids.contains(&controller_user_id) {
        return Ok(SessionAuthz::allow(mode));
    }
    let role_ids = state
        .tenants
        .member_role_ids(agent.tenant_id, controller_user_id)
        .await
        .unwrap_or_default();
    if policy.allowed_role_ids.iter().any(|r| role_ids.contains(r)) {
        return Ok(SessionAuthz::allow(mode));
    }
    Err("you're not on this device's control allowlist".to_string())
}

/// Stable short code for the wire. Exhaustive match so a new
/// `remote_control::Error` variant triggers a compile error here rather
/// than silently being reported as "internal".
fn error_code(e: &roomler_ai_remote_control::Error) -> &'static str {
    use roomler_ai_remote_control::Error::*;
    match e {
        AgentOffline(_) => "agent_offline",
        AgentNotFound(_) => "agent_not_found",
        AgentBusy => "agent_busy",
        SessionNotFound(_) => "session_not_found",
        BadPhase(_, _) => "bad_phase",
        ConsentDenied => "consent_denied",
        ConsentTimeout => "consent_timeout",
        PermissionDenied(_) => "permission_denied",
        BadMessage(_) => "bad_message",
        SendFailed => "send_failed",
        Mongo(_) => "internal",
        Bson(_) => "internal",
        Json(_) => "internal",
    }
}

/// If the underlying error references a specific session, extract its id
/// so the controller UI can route the error to the right spinner instead
/// of assuming it's about the most recently attempted session.
fn error_session_id(e: &roomler_ai_remote_control::Error) -> Option<bson::oid::ObjectId> {
    use roomler_ai_remote_control::Error::*;
    match e {
        SessionNotFound(hex) => bson::oid::ObjectId::parse_str(hex).ok(),
        BadPhase(hex, _) => bson::oid::ObjectId::parse_str(hex).ok(),
        _ => None,
    }
}

/// Intercept tunnel-flow `ClientMsg` variants from the agent and route
/// the corresponding `ServerMsg` to the registered tunnel-client (if
/// any) keyed by `session_id`. Non-tunnel variants are returned
/// unchanged so the caller can pass them to the Hub.
///
/// Returns `None` if the message was consumed by the tunnel relay
/// (don't dispatch to the Hub afterwards), or `Some(parsed)` if the
/// caller should continue with Hub dispatch.
async fn relay_tunnel_msg_from_agent(state: &AppState, parsed: ClientMsg) -> Option<ClientMsg> {
    match parsed {
        ClientMsg::TcpForwardAccept {
            session_id,
            flow_id,
            dc_index,
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TcpForwardAccept {
                    session_id,
                    flow_id,
                    dc_index,
                },
            )
            .await;
            None
        }
        ClientMsg::TcpForwardReject {
            session_id,
            flow_id,
            kind,
            reason,
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TcpForwardReject {
                    session_id,
                    flow_id,
                    kind,
                    reason,
                },
            )
            .await;
            None
        }
        ClientMsg::TcpHalfClose {
            session_id,
            flow_id,
            direction,
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TcpHalfClose {
                    session_id,
                    flow_id,
                    direction,
                },
            )
            .await;
            None
        }
        ClientMsg::TcpClosed {
            session_id,
            flow_id,
            reason,
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TcpClosed {
                    session_id,
                    flow_id,
                    reason,
                },
            )
            .await;
            None
        }
        ClientMsg::TunnelTerminate { session_id, reason } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TunnelTerminate { session_id, reason },
            )
            .await;
            None
        }
        ClientMsg::TunnelSdpAnswer { session_id, sdp } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TunnelSdpAnswer { session_id, sdp },
            )
            .await;
            None
        }
        ClientMsg::TunnelIce {
            session_id,
            candidate,
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TunnelIce {
                    session_id,
                    candidate,
                },
            )
            .await;
            None
        }
        // Phase 1c: the agent's QUIC endpoint is up — relay its cert
        // fingerprint (for the client to pin) + dialable addrs to the
        // tunnel-client so it can connect the direct P2P QUIC link.
        ClientMsg::TunnelQuicReady {
            session_id,
            cert_fingerprint,
            addrs,
        } => {
            relay_to_client(
                state,
                session_id,
                ServerMsg::TunnelQuicReady {
                    session_id,
                    cert_fingerprint,
                    addrs,
                },
            )
            .await;
            None
        }
        // `TunnelHello` / `TunnelOpen` / `TcpForwardRequest` /
        // `TunnelSdpOffer` are tunnel-client → server messages;
        // agents shouldn't emit them. Pass through to the Hub so a
        // misbehaving agent gets a `bad_message` rather than being
        // silently dropped.
        other => Some(other),
    }
}

/// Push a `ServerMsg` to the tunnel-client registered for
/// `session_id`. No-op when the client has gone away (peer torn
/// down between agent emit + relay).
async fn relay_to_client(state: &AppState, session_id: bson::oid::ObjectId, msg: ServerMsg) {
    let Some(tx) = state
        .tunnel_clients_by_session
        .get(&session_id)
        .map(|entry| entry.value().clone())
    else {
        debug!(%session_id, "agent → client relay: no registered tunnel-client; dropping");
        return;
    };
    if let Err(e) = tx.send(msg).await {
        debug!(%session_id, %e, "agent → client relay: channel closed");
    }
}

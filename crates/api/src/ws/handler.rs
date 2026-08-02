use axum::{
    extract::{
        Query, State, WebSocketUpgrade,
        ws::{CloseFrame, Message, WebSocket},
    },
    response::Response,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use bson::oid::ObjectId;
use futures::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use mediasoup::prelude::*;
use serde::Deserialize;
use sha1::Sha1;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct WsParams {
    pub token: String,
    /// Optional connection role. Defaults to `"user"` to preserve existing
    /// browser behaviour. Set to `"agent"` by the native remote-control agent.
    #[serde(default)]
    pub role: Option<String>,
    /// S6 — optional tenant-affinity key. The front load-balancer hashes
    /// on it so one tenant's users, agents, tunnel clients and DERP
    /// sockets co-locate on one pod (the in-memory rc-hub / tunnel-hub /
    /// mediasoup state is pod-local). The SERVER only validates it:
    /// agent/tunnel JWTs carry `tenant_id` → must match; user JWTs carry
    /// no tenant claim → validated via a `tenant_members` lookup.
    /// Absent = legacy client → accepted (the LB pins those to pod 1).
    #[serde(default)]
    pub tid: Option<String>,
}

/// Validate a claimed affinity `tid` against a token-derived tenant id.
/// `None` (param absent) is always fine — validation only rejects a
/// PRESENT-but-wrong claim (mis-routed or forged affinity key).
fn tid_matches_claim(tid: &Option<String>, claim_tenant_hex: &str) -> bool {
    match tid {
        None => true,
        Some(t) => t == claim_tenant_hex,
    }
}

pub async fn ws_upgrade(
    State(state): State<AppState>,
    Query(params): Query<WsParams>,
    ws: WebSocketUpgrade,
) -> Response {
    match params.role.as_deref() {
        Some("agent") => ws_upgrade_agent(state, params.token, params.tid, ws),
        Some("tunnel-client") => ws_upgrade_tunnel_client(state, params.token, params.tid, ws),
        _ => ws_upgrade_user(state, params.token, params.tid, ws).await,
    }
}

async fn ws_upgrade_user(
    state: AppState,
    token: String,
    tid: Option<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let claims = match state.auth.verify_access_token(&token) {
        Ok(c) => c,
        Err(_) => {
            return Response::builder()
                .status(401)
                .body("Unauthorized".into())
                .unwrap();
        }
    };

    let user_id = match ObjectId::parse_str(&claims.sub) {
        Ok(id) => id,
        Err(_) => {
            return Response::builder()
                .status(400)
                .body("Invalid user ID".into())
                .unwrap();
        }
    };

    // S6 — user JWTs carry no tenant claim, so a present `tid` is
    // validated against `tenant_members`. Rejecting a non-member claim
    // matters: `tid` is attacker-choosable in the URL, and accepting an
    // arbitrary value would let a user steer their affinity onto any
    // tenant's pod (harmless for data access — every route re-checks
    // membership — but it would poison the co-location guarantee).
    if let Some(t) = &tid {
        let tenant_ok = match ObjectId::parse_str(t) {
            Ok(tenant_oid) => state
                .tenants
                .is_member(tenant_oid, user_id)
                .await
                .unwrap_or(false),
            Err(_) => false,
        };
        if !tenant_ok {
            return Response::builder()
                .status(403)
                .body("Not a member of the claimed tenant".into())
                .unwrap();
        }
    }

    let username = claims.username.clone();

    ws.on_upgrade(move |socket| handle_socket(socket, state, user_id, username))
}

fn ws_upgrade_tunnel_client(
    state: AppState,
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

    ws.on_upgrade(move |socket| async move {
        // Connect-time revocation check. Periodic re-check (every 60 s)
        // lives in `ws::tunnel::handle_tunnel_client_socket`.
        let client = match state
            .tunnel_clients
            .find_in_tenant(tenant_id, tunnel_client_id)
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
        crate::ws::tunnel::handle_tunnel_client_socket(
            state,
            socket,
            tunnel_client_id,
            tenant_id,
            owner_user_id,
        )
        .await;
    })
}

fn ws_upgrade_agent(
    state: AppState,
    token: String,
    tid: Option<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let claims = match state.auth.verify_agent_token(&token) {
        Ok(c) => c,
        Err(_) => {
            return Response::builder()
                .status(401)
                .body("Unauthorized (agent)".into())
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

    let agent_id = match ObjectId::parse_str(&claims.sub) {
        Ok(id) => id,
        Err(_) => {
            return Response::builder()
                .status(400)
                .body("Invalid agent ID".into())
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

    ws.on_upgrade(move |socket| async move {
        // Verify the agent still exists and isn't quarantined/deleted before
        // we pump any signalling. One Mongo read per connect is cheap and
        // gives us a clean revocation story without needing a token blacklist.
        let agent = match state.agents.find_in_tenant(tenant_id, agent_id).await {
            Ok(a) => a,
            Err(e) => {
                warn!(%agent_id, %e, "agent lookup failed on WS connect");
                return;
            }
        };
        if agent.deleted_at.is_some()
            || matches!(
                agent.status,
                roomler_ai_remote_control::models::AgentStatus::Quarantined
            )
        {
            // rc.53: push a `ServerMsg::Goodbye { reason: AgentDeleted }`
            // text frame + a Close frame BEFORE dropping the socket so
            // the agent can log a useful "your row was deleted, re-enrol"
            // line instead of an opaque `ws read` (the failure mode
            // CORPLAP-2 wedged on for hours pre-rc.53). The agent's
            // `handle_server_msg::ServerMsg::Goodbye` arm decides this is
            // fatal + exits with `AGENT_DELETED_EXIT_CODE = 7`, which
            // the SCM supervisor's rc.53 code-7 fast-alarm fires on the
            // FIRST exit.
            info!(%agent_id, "agent is quarantined or deleted; refusing WS with rc:goodbye");
            let goodbye = roomler_ai_remote_control::signaling::ServerMsg::Goodbye {
                reason: roomler_ai_remote_control::signaling::AgentCloseReason::AgentDeleted,
                message: "This agent's server-side row was deleted (or quarantined). \
                          Re-enrol with a fresh enrollment token from the admin UI to \
                          revive (soft-deleted rows rehydrate by (tenant_id, machine_id))."
                    .into(),
            };
            send_goodbye_and_close(socket, &goodbye, 4003, "agent_deleted").await;
            return;
        }
        crate::ws::remote_control::handle_agent_socket(
            state,
            socket,
            agent_id,
            tenant_id,
            agent.owner_user_id,
        )
        .await;
    })
}

/// rc.53: push a server-initiated close frame WITH a structured
/// `ServerMsg` text frame in front of it, so the agent / tunnel-client
/// learns WHY the connection is being closed before the socket
/// drops. Used at the WS-handler refusal sites (`ws_upgrade_agent` /
/// `ws_upgrade_tunnel_client`) when the row is deleted or
/// quarantined.
///
/// Tungstenite serialises both frames into the same outbound TCP
/// buffer; the OS delivers them in order. No `sleep` guard is needed
/// — the `.await` on `send(Close)` completes the underlying flush.
/// On send-error we still attempt the close so the agent at least
/// gets the TCP FIN; on close-send-error we just drop (the socket
/// is already dead, nothing further to do).
async fn send_goodbye_and_close<M: serde::Serialize>(
    socket: WebSocket,
    msg: &M,
    close_code: u16,
    close_reason: &str,
) {
    let mut socket = socket;
    let json = match serde_json::to_string(msg) {
        Ok(s) => s,
        Err(e) => {
            warn!(%e, "failed to serialise Goodbye payload; closing without it");
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };
    if let Err(e) = socket.send(Message::Text(json.into())).await {
        warn!(%e, "Goodbye text-send failed; attempting close anyway");
    }
    if let Err(e) = socket
        .send(Message::Close(Some(CloseFrame {
            code: close_code,
            reason: close_reason.to_string().into(),
        })))
        .await
    {
        debug!(%e, "Goodbye close-send failed (socket may already be dropped)");
    }
    // socket drops here; tungstenite has flushed both frames into
    // the outbound TCP buffer before the close-send `.await`
    // returned.
}

async fn handle_socket(socket: WebSocket, state: AppState, user_id: ObjectId, username: String) {
    let connection_id = Uuid::new_v4().to_string();
    info!(?user_id, %connection_id, "WebSocket connected");

    let (sender, mut receiver) = socket.split();
    let sender = Arc::new(Mutex::new(sender));

    state
        .ws_storage
        .add(user_id, connection_id.clone(), sender.clone());

    // S6 — mirror into the cross-pod online registry (advisory; the 30 s
    // heartbeat in main.rs re-asserts it and the 90 s TTL self-heals).
    if let Some(pubsub) = &state.redis_pubsub
        && let Err(e) = pubsub.online_add(&user_id.to_hex()).await
    {
        tracing::debug!("online-registry add failed: {e}");
    }

    // Register this tab with the remote-control Hub so `rc:*` replies find us.
    // Each browser tab gets its own controller tx; the Hub routes by tx, not
    // by user id, so multiple tabs don't cross signals.
    let (rc_controller_tx, rc_controller_rx) = state.rc_hub.register_controller(user_id);
    let rc_pump = tokio::spawn(crate::ws::remote_control::pump_server_messages(
        rc_controller_rx,
        sender.clone(),
    ));

    {
        let msg = serde_json::json!({
            "type": "connected",
            "user_id": user_id.to_hex(),
        });
        let mut guard = sender.lock().await;
        let _ = guard
            .send(Message::text(serde_json::to_string(&msg).unwrap()))
            .await;
    }

    // Resolve a human-friendly display name for this controller once per
    // connection. It's the identity shown to the viewee — the "Being
    // viewed by" overlay caption + its initials avatar — and in owner
    // approval notifications. The JWT only carries the login username, so
    // look up the profile's display_name here, falling back to the
    // username when it's unset.
    let controller_display = match state.users.base.find_by_id(user_id).await {
        Ok(u) if !u.display_name.trim().is_empty() => u.display_name,
        _ => username,
    };

    while let Some(msg) = receiver.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                handle_client_message(
                    &state,
                    &user_id,
                    &connection_id,
                    &controller_display,
                    &rc_controller_tx,
                    &text,
                )
                .await;
            }
            Ok(Message::Ping(data)) => {
                let mut guard = sender.lock().await;
                let _ = guard.send(Message::Pong(data)).await;
            }
            Ok(Message::Close(_)) => {
                break;
            }
            Err(e) => {
                warn!(?user_id, %connection_id, %e, "WebSocket error");
                break;
            }
            _ => {}
        }
    }

    // Cleanup
    state
        .rc_hub
        .unregister_controller(user_id, &rc_controller_tx);
    rc_pump.abort();
    state.ws_storage.remove(&user_id, &connection_id, &sender);

    // S6 — drop this pod's online-registry claim once the user's LAST
    // local connection is gone (other pods' claims are theirs to clear).
    if !state.ws_storage.is_connected(&user_id)
        && let Some(pubsub) = &state.redis_pubsub
        && let Err(e) = pubsub.online_remove(&user_id.to_hex()).await
    {
        tracing::debug!("online-registry remove failed: {e}");
    }

    // C-4 — the conn may have joined a room OWNED BY ANOTHER POD: tell
    // the owner to drop its transports (best-effort; no-op when the map
    // has no entry).
    crate::ws::media_cluster::forward_close_leave(&state, &user_id, &connection_id).await;

    if let Some(room_id) = state.room_manager.get_connection_room(&connection_id) {
        let remaining_conns = state
            .room_manager
            .get_other_connection_ids(&room_id, &connection_id);

        state
            .room_manager
            .close_participant(&room_id, &connection_id);

        if !remaining_conns.is_empty() {
            let event = serde_json::json!({
                "type": "media:peer_left",
                "data": {
                    "user_id": user_id.to_hex(),
                    "connection_id": connection_id,
                    "room_id": room_id.to_hex(),
                }
            });
            for conn_id in &remaining_conns {
                super::dispatcher::send_to_connection_routed(
                    &state.ws_storage,
                    &state.redis_pubsub,
                    conn_id,
                    &event,
                )
                .await;
            }
        }
    }

    info!(?user_id, %connection_id, "WebSocket disconnected");
}

async fn handle_client_message(
    state: &AppState,
    user_id: &ObjectId,
    connection_id: &str,
    username: &str,
    rc_controller_tx: &roomler_ai_remote_control::session::ClientTx,
    text: &str,
) {
    // Remote-control messages use a `t` discriminator prefixed with "rc:".
    // Peek at the raw JSON before full parse so we don't pay the cost on
    // every media/presence message.
    if text.contains("\"rc:") {
        // Authorization + consent-mode gate for `rc:session.request`
        // (self-control / admin / REMOTE_CONTROL + per-device allowlist +
        // quarantine). A non-request rc:* message resolves to `Ok(Prompt)` and
        // falls straight through to dispatch (the mode is unused for it).
        let authz =
            match crate::ws::remote_control::resolve_session_authz(state, *user_id, text).await {
                Ok(a) => a,
                Err(reason) => {
                    warn!(?user_id, %reason, "rc:session.request denied by authz gate");
                    let _ = rc_controller_tx.try_send(
                        roomler_ai_remote_control::signaling::ServerMsg::Error {
                            session_id: None,
                            code: "permission_denied".to_string(),
                            message: reason,
                            open_nonce: None,
                        },
                    );
                    return;
                }
            };
        let handled = crate::ws::remote_control::dispatch_controller_rc(
            state,
            *user_id,
            username,
            rc_controller_tx,
            text,
            authz.mode,
            authz.override_reason,
        )
        .await;
        if handled {
            return;
        }
    }

    let parsed: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return,
    };

    let msg_type = parsed.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let data = parsed.get("data");

    debug!(?user_id, %connection_id, msg_type, "WS message received");

    match msg_type {
        "ping" => {
            let pong = serde_json::json!({ "type": "pong" });
            super::dispatcher::send_to_user(&state.ws_storage, user_id, &pong).await;
        }
        "typing:start" | "typing:stop" => {
            if let Some(room_id_str) = data.and_then(|d| d.get("room_id")).and_then(|c| c.as_str())
                && let Ok(rid) = ObjectId::parse_str(room_id_str)
                && let Ok(member_ids) = state.rooms.find_member_user_ids(rid).await
            {
                let recipients: Vec<ObjectId> =
                    member_ids.into_iter().filter(|id| id != user_id).collect();
                let event = serde_json::json!({
                    "type": msg_type,
                    "data": {
                        "room_id": room_id_str,
                        "user_id": user_id.to_hex(),
                    }
                });
                super::dispatcher::broadcast_with_redis(
                    &state.ws_storage,
                    &state.redis_pubsub,
                    &recipients,
                    &event,
                )
                .await;
            }
        }
        "presence:update" => {
            if let Some(presence) = data
                .and_then(|d| d.get("presence"))
                .and_then(|p| p.as_str())
            {
                let event = serde_json::json!({
                    "type": "presence:update",
                    "data": {
                        "user_id": user_id.to_hex(),
                        "presence": presence,
                    }
                });
                // S6 — broadcast-flag envelope: each pod delivers to its
                // OWN local users. The previous recipient list was this
                // pod's `all_user_ids()`, which silently excluded every
                // user connected to the other pod.
                super::dispatcher::broadcast_all_with_redis(
                    &state.ws_storage,
                    &state.redis_pubsub,
                    &event,
                )
                .await;
            }
        }
        t if t.starts_with("media:") => {
            route_and_dispatch_media(state, user_id, connection_id, t, data).await;
        }
        _ => {
            debug!(?user_id, msg_type, "Unknown WS message type");
        }
    }
}

/// C-4 — the claim-or-route placement gate for every `media:*` command:
/// a locally-held room serves in place (today's path); a foreign-owned
/// room gets the command forwarded to its owner over the bus, with
/// replies/pushes returning conn-addressed on the global channel. Only
/// the join path may materialize a room (`create` from the resolver).
async fn route_and_dispatch_media(
    state: &AppState,
    user_id: &ObjectId,
    connection_id: &str,
    msg_type: &str,
    data: Option<&serde_json::Value>,
) {
    use crate::ws::media_cluster::{MediaRoute, forward_media_cmd, resolve_media_route};

    let rid = data
        .and_then(|d| d.get("room_id"))
        .and_then(|v| v.as_str())
        .and_then(|s| ObjectId::parse_str(s).ok());
    let Some(rid) = rid else {
        // Missing/invalid room_id — the local handlers own the error shape.
        dispatch_media_local(state, user_id, connection_id, msg_type, data, false).await;
        return;
    };
    let is_join = msg_type == "media:join";
    match resolve_media_route(state, &rid, is_join).await {
        MediaRoute::Local { create } => {
            // A conn whose room came home (fold/rehome) no longer needs
            // close-time forwarding to a remote owner.
            state.remote_media_conns.remove(connection_id);
            dispatch_media_local(state, user_id, connection_id, msg_type, data, create).await;
        }
        MediaRoute::Remote(owner) => {
            let ok = forward_media_cmd(state, &owner, user_id, connection_id, msg_type, data).await;
            // Track remote membership so WS close can tell the owner to
            // drop the transports. (The prune fallback may have served
            // the join LOCALLY after all — skip the marker then.)
            if is_join && ok && !state.room_manager.has_room(&rid) {
                state
                    .remote_media_conns
                    .insert(connection_id.to_string(), rid);
            } else if msg_type == "media:leave" {
                state.remote_media_conns.remove(connection_id);
            }
        }
    }
}

/// Execute one media command against the LOCAL room manager. Shared by
/// the WS dispatch above and the owner-side `media.cmd` bus handler
/// (`ws/media_cluster.rs`) — which is why it must never re-enter the
/// placement gate.
pub(crate) async fn dispatch_media_local(
    state: &AppState,
    user_id: &ObjectId,
    connection_id: &str,
    msg_type: &str,
    data: Option<&serde_json::Value>,
    create_allowed: bool,
) {
    match msg_type {
        "media:join" => {
            handle_media_join(state, user_id, connection_id, data, create_allowed).await;
        }
        "media:connect_transport" => {
            handle_media_connect_transport(state, connection_id, data).await;
        }
        "media:produce" => {
            handle_media_produce(state, user_id, connection_id, data).await;
        }
        "media:consume" => {
            handle_media_consume(state, user_id, connection_id, data).await;
        }
        "media:producer_close" => {
            handle_media_producer_close(state, user_id, connection_id, data).await;
        }
        "media:leave" => {
            handle_media_leave(state, user_id, connection_id, data).await;
        }
        "media:play_audio" => {
            handle_play_audio(state, user_id, connection_id, data).await;
        }
        "media:stop_audio" => {
            handle_stop_audio(state, user_id, connection_id, data).await;
        }
        _ => {
            debug!(?user_id, msg_type, "Unknown media message type");
        }
    }
}

async fn send_media_error(state: &AppState, connection_id: &str, message: &str) {
    let msg = serde_json::json!({
        "type": "media:error",
        "data": { "message": message }
    });
    // Conn-addressed + routed: an error raised while executing a
    // forwarded command must reach the viewer's pod.
    super::dispatcher::send_to_connection_routed(
        &state.ws_storage,
        &state.redis_pubsub,
        connection_id,
        &msg,
    )
    .await;
}

async fn handle_media_join(
    state: &AppState,
    user_id: &ObjectId,
    connection_id: &str,
    data: Option<&serde_json::Value>,
    create_allowed: bool,
) {
    let room_id_str = match data.and_then(|d| d.get("room_id")).and_then(|c| c.as_str()) {
        Some(s) => s,
        None => {
            send_media_error(state, connection_id, "Missing room_id").await;
            return;
        }
    };

    let rid = match ObjectId::parse_str(room_id_str) {
        Ok(id) => id,
        Err(_) => {
            send_media_error(state, connection_id, "Invalid room_id").await;
            return;
        }
    };

    let mut room_exists = state.room_manager.has_room(&rid);
    debug!(?user_id, %connection_id, ?rid, room_exists, create_allowed, "media:join room check");
    if !room_exists && create_allowed {
        // C-4 — materialization is claim-gated: `create_allowed` comes
        // from the placement resolver (NX claim won, or the belt
        // fallback with a live conference while the directory is down).
        // The Mongo in_progress check lives in the resolver too.
        match state.room_manager.create_room(rid).await {
            Ok(_) => {
                info!(?rid, "media:join materialized conference room on this pod");
                room_exists = true;
            }
            Err(e) => {
                warn!(?rid, %e, "media:join room create failed");
            }
        }
    }
    if !room_exists {
        send_media_error(state, connection_id, "Room does not exist").await;
        return;
    }

    let transport_pair = match state
        .room_manager
        .create_transports(rid, *user_id, connection_id.to_string())
        .await
    {
        Ok(tp) => tp,
        Err(e) => {
            send_media_error(
                state,
                user_id,
                &format!("Failed to create transports: {}", e),
            )
            .await;
            return;
        }
    };

    if let Some(room) = state.room_manager.rooms_ref().get(&rid) {
        let caps = serde_json::to_value(room.router.rtp_capabilities()).unwrap_or_default();
        let msg = serde_json::json!({
            "type": "media:router_capabilities",
            "data": { "rtp_capabilities": caps }
        });
        super::dispatcher::send_to_connection_routed(
            &state.ws_storage,
            &state.redis_pubsub,
            connection_id,
            &msg,
        )
        .await;
    }

    let ice_servers: Vec<serde_json::Value> = if let Some(ref url) = state.settings.turn.url {
        let (turn_username, turn_credential) =
            if let Some(ref secret) = state.settings.turn.shared_secret {
                let expiry = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + 86400;
                let username = format!("{}:{}", expiry, user_id.to_hex());
                let mut mac = Hmac::<Sha1>::new_from_slice(secret.as_bytes())
                    .expect("HMAC key length is valid");
                mac.update(username.as_bytes());
                let credential = BASE64.encode(mac.finalize().into_bytes());
                debug!(%username, "Generated TURN ephemeral credentials");
                (username, credential)
            } else {
                (
                    state
                        .settings
                        .turn
                        .username
                        .as_deref()
                        .unwrap_or("")
                        .to_string(),
                    state
                        .settings
                        .turn
                        .password
                        .as_deref()
                        .unwrap_or("")
                        .to_string(),
                )
            };
        // Build TURN URLs with multiple transport variants.
        // UDP TURN often fails behind NAT/firewalls, so include TCP and TLS
        // fallbacks. Also emit `turn:HOST:443?transport=udp` because many
        // corporate firewalls allow UDP/443 (QUIC) but block UDP/3478.
        let mut urls: Vec<String> = vec![url.clone()];
        if url.starts_with("turn:") && !url.contains("?transport=") {
            let turn_443 = url.replace(":3478", ":443");
            urls.push(format!("{}?transport=udp", turn_443));
            urls.push(format!("{}?transport=tcp", url));
            // Derive TURNS (TLS) URL on port 5349
            let turns_url = url.replacen("turn:", "turns:", 1).replace(":3478", ":5349");
            urls.push(format!("{}?transport=tcp", turns_url));
        }
        vec![serde_json::json!({
            "urls": urls,
            "username": turn_username,
            "credential": turn_credential,
        })]
    } else {
        vec![]
    };

    let force_relay = state.settings.turn.force_relay.unwrap_or(false);

    if force_relay {
        info!("force_relay=true — clients will use iceTransportPolicy='relay' via TURN server");
    }

    info!(
        %connection_id,
        force_relay,
        announced_ip = %state.settings.mediasoup.announced_ip,
        turn_url = ?state.settings.turn.url,
        send_ice_candidates = %transport_pair.send_transport.ice_candidates,
        recv_ice_candidates = %transport_pair.recv_transport.ice_candidates,
        "media:join transport_created ICE diagnostics"
    );

    let msg = serde_json::json!({
        "type": "media:transport_created",
        "data": {
            "send_transport": transport_pair.send_transport,
            "recv_transport": transport_pair.recv_transport,
            "ice_servers": ice_servers,
            "force_relay": force_relay,
        }
    });
    super::dispatcher::send_to_connection_routed(
        &state.ws_storage,
        &state.redis_pubsub,
        connection_id,
        &msg,
    )
    .await;

    let producers = state.room_manager.get_producer_ids(&rid, connection_id);
    for (uid, conn_id, pid, kind, source) in producers {
        let msg = serde_json::json!({
            "type": "media:new_producer",
            "data": {
                "producer_id": pid.to_string(),
                "user_id": uid.to_hex(),
                "connection_id": conn_id,
                "kind": match kind { MediaKind::Audio => "audio", MediaKind::Video => "video" },
                "source": source,
            }
        });
        super::dispatcher::send_to_connection_routed(
            &state.ws_storage,
            &state.redis_pubsub,
            connection_id,
            &msg,
        )
        .await;
    }
}

async fn handle_media_connect_transport(
    state: &AppState,
    connection_id: &str,
    data: Option<&serde_json::Value>,
) {
    let data = match data {
        Some(d) => d,
        None => return,
    };

    let room_id_str = match data.get("room_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return,
    };
    let transport_id = match data.get("transport_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return,
    };
    let dtls_parameters: DtlsParameters = match data
        .get("dtls_parameters")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(p) => p,
        None => return,
    };

    let rid = match ObjectId::parse_str(room_id_str) {
        Ok(id) => id,
        Err(_) => return,
    };

    if let Err(e) = state
        .room_manager
        .connect_transport(&rid, connection_id, transport_id, dtls_parameters)
        .await
    {
        warn!(%connection_id, %e, "connect_transport failed");
    }
}

async fn handle_media_produce(
    state: &AppState,
    user_id: &ObjectId,
    connection_id: &str,
    data: Option<&serde_json::Value>,
) {
    let data = match data {
        Some(d) => d,
        None => {
            send_media_error(state, connection_id, "Missing data").await;
            return;
        }
    };

    let room_id_str = match data.get("room_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            send_media_error(state, connection_id, "Missing room_id").await;
            return;
        }
    };
    let kind: MediaKind = match data
        .get("kind")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(k) => k,
        None => {
            send_media_error(state, connection_id, "Invalid kind").await;
            return;
        }
    };
    let rtp_parameters: RtpParameters = match data
        .get("rtp_parameters")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(p) => p,
        None => {
            send_media_error(state, connection_id, "Invalid rtp_parameters").await;
            return;
        }
    };
    let source = data
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or(match kind {
            MediaKind::Audio => "audio",
            MediaKind::Video => "camera",
        })
        .to_string();

    let rid = match ObjectId::parse_str(room_id_str) {
        Ok(id) => id,
        Err(_) => {
            send_media_error(state, connection_id, "Invalid room_id").await;
            return;
        }
    };

    match state
        .room_manager
        .produce(&rid, connection_id, kind, rtp_parameters, source.clone())
        .await
    {
        Ok(producer_id) => {
            let result_msg = serde_json::json!({
                "type": "media:produce_result",
                "data": { "id": producer_id.to_string() }
            });
            super::dispatcher::send_to_connection_routed(
                &state.ws_storage,
                &state.redis_pubsub,
                connection_id,
                &result_msg,
            )
            .await;

            let other_conns = state
                .room_manager
                .get_other_connection_ids(&rid, connection_id);

            if !other_conns.is_empty() {
                let event = serde_json::json!({
                    "type": "media:new_producer",
                    "data": {
                        "producer_id": producer_id.to_string(),
                        "user_id": user_id.to_hex(),
                        "connection_id": connection_id,
                        "kind": match kind { MediaKind::Audio => "audio", MediaKind::Video => "video" },
                        "source": source,
                    }
                });
                for conn_id in &other_conns {
                    super::dispatcher::send_to_connection_routed(
                        &state.ws_storage,
                        &state.redis_pubsub,
                        conn_id,
                        &event,
                    )
                    .await;
                }
            }
        }
        Err(e) => {
            send_media_error(state, connection_id, &format!("produce failed: {}", e)).await;
        }
    }
}

async fn handle_media_consume(
    state: &AppState,
    user_id: &ObjectId,
    connection_id: &str,
    data: Option<&serde_json::Value>,
) {
    let data = match data {
        Some(d) => d,
        None => {
            send_media_error(state, connection_id, "Missing data").await;
            return;
        }
    };

    let room_id_str = match data.get("room_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            send_media_error(state, connection_id, "Missing room_id").await;
            return;
        }
    };
    let producer_id_str = match data.get("producer_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            send_media_error(state, connection_id, "Missing producer_id").await;
            return;
        }
    };
    let rtp_capabilities: RtpCapabilities = match data
        .get("rtp_capabilities")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(c) => c,
        None => {
            send_media_error(state, connection_id, "Invalid rtp_capabilities").await;
            return;
        }
    };

    let rid = match ObjectId::parse_str(room_id_str) {
        Ok(id) => id,
        Err(_) => {
            send_media_error(state, connection_id, "Invalid room_id").await;
            return;
        }
    };

    let producer_id = match producer_id_str.parse::<ProducerId>() {
        Ok(id) => id,
        Err(_) => {
            send_media_error(state, connection_id, "Invalid producer_id").await;
            return;
        }
    };

    match state
        .room_manager
        .consume(&rid, connection_id, producer_id, &rtp_capabilities)
        .await
    {
        Ok(consumer_info) => {
            let msg = serde_json::json!({
                "type": "media:consumer_created",
                "data": {
                    "id": consumer_info.id,
                    "producer_id": consumer_info.producer_id,
                    "kind": consumer_info.kind,
                    "rtp_parameters": consumer_info.rtp_parameters,
                }
            });
            super::dispatcher::send_to_connection_routed(
                &state.ws_storage,
                &state.redis_pubsub,
                connection_id,
                &msg,
            )
            .await;
        }
        Err(e) => {
            send_media_error(state, connection_id, &format!("consume failed: {}", e)).await;
        }
    }
}

async fn handle_media_producer_close(
    state: &AppState,
    user_id: &ObjectId,
    connection_id: &str,
    data: Option<&serde_json::Value>,
) {
    let data = match data {
        Some(d) => d,
        None => return,
    };

    let room_id_str = match data.get("room_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return,
    };
    let producer_id_str = match data.get("producer_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return,
    };

    let rid = match ObjectId::parse_str(room_id_str) {
        Ok(id) => id,
        Err(_) => return,
    };

    let producer_id = match producer_id_str.parse::<ProducerId>() {
        Ok(id) => id,
        Err(_) => return,
    };

    if state
        .room_manager
        .close_producer(&rid, connection_id, &producer_id)
    {
        state
            .room_manager
            .remove_rtp_tap(&rid, &producer_id.to_string());

        let other_conns = state
            .room_manager
            .get_other_connection_ids(&rid, connection_id);

        if !other_conns.is_empty() {
            let event = serde_json::json!({
                "type": "media:producer_closed",
                "data": {
                    "producer_id": producer_id.to_string(),
                    "user_id": user_id.to_hex(),
                }
            });
            for conn_id in &other_conns {
                super::dispatcher::send_to_connection_routed(
                    &state.ws_storage,
                    &state.redis_pubsub,
                    conn_id,
                    &event,
                )
                .await;
            }
        }
    }
}

async fn handle_media_leave(
    state: &AppState,
    user_id: &ObjectId,
    connection_id: &str,
    data: Option<&serde_json::Value>,
) {
    let room_id_str = match data.and_then(|d| d.get("room_id")).and_then(|c| c.as_str()) {
        Some(s) => s,
        None => return,
    };

    let rid = match ObjectId::parse_str(room_id_str) {
        Ok(id) => id,
        Err(_) => return,
    };

    let other_conns = state
        .room_manager
        .get_other_connection_ids(&rid, connection_id);

    state.room_manager.close_participant(&rid, connection_id);

    if !other_conns.is_empty() {
        let event = serde_json::json!({
            "type": "media:peer_left",
            "data": {
                "user_id": user_id.to_hex(),
                "connection_id": connection_id,
                "room_id": rid.to_hex(),
            }
        });
        for conn_id in &other_conns {
            super::dispatcher::send_to_connection_routed(
                &state.ws_storage,
                &state.redis_pubsub,
                conn_id,
                &event,
            )
            .await;
        }
    }
}

async fn handle_play_audio(
    state: &AppState,
    _user_id: &ObjectId,
    connection_id: &str,
    data: Option<&serde_json::Value>,
) {
    let data = match data {
        Some(d) => d,
        None => return,
    };

    let room_id_str = match data.get("room_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return,
    };
    let file_id = match data.get("file_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return,
    };

    let rid = match ObjectId::parse_str(room_id_str) {
        Ok(id) => id,
        Err(_) => return,
    };
    let fid = match ObjectId::parse_str(file_id) {
        Ok(id) => id,
        Err(_) => return,
    };

    // Look up the room to get tenant_id
    let room = match state.rooms.base.find_by_id(rid).await {
        Ok(r) => r,
        Err(e) => {
            warn!(%e, "Failed to find room for file playback");
            return;
        }
    };

    let file = match state
        .files
        .base
        .find_by_id_in_tenant(room.tenant_id, fid)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            warn!(%e, "Failed to find file for playback");
            return;
        }
    };

    let playback_id = String::new();

    let file_url = format!(
        "/api/tenant/{}/file/{}/download",
        room.tenant_id.to_hex(),
        fid.to_hex(),
    );
    let msg = serde_json::json!({
        "type": "media:audio_playback",
        "data": {
            "action": "start",
            "file_url": file_url,
            "file_id": file_id,
            "filename": file.filename,
            "playback_id": playback_id,
            "room_id": room_id_str,
        }
    });

    super::dispatcher::send_to_connection_routed(
        &state.ws_storage,
        &state.redis_pubsub,
        connection_id,
        &msg,
    )
    .await;
    let other_conns = state
        .room_manager
        .get_other_connection_ids(&rid, connection_id);
    for cid in &other_conns {
        super::dispatcher::send_to_connection_routed(
            &state.ws_storage,
            &state.redis_pubsub,
            cid,
            &msg,
        )
        .await;
    }

    info!(%rid, %file_id, %playback_id, "Audio playback started");
}

async fn handle_stop_audio(
    state: &AppState,
    _user_id: &ObjectId,
    connection_id: &str,
    data: Option<&serde_json::Value>,
) {
    let data = match data {
        Some(d) => d,
        None => return,
    };

    let room_id_str = match data.get("room_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return,
    };
    let playback_id = match data.get("playback_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return,
    };

    let rid = match ObjectId::parse_str(room_id_str) {
        Ok(id) => id,
        Err(_) => return,
    };

    let msg = serde_json::json!({
        "type": "media:audio_playback",
        "data": {
            "action": "stop",
            "playback_id": playback_id,
            "room_id": room_id_str,
        }
    });

    super::dispatcher::send_to_connection_routed(
        &state.ws_storage,
        &state.redis_pubsub,
        connection_id,
        &msg,
    )
    .await;
    let other_conns = state
        .room_manager
        .get_other_connection_ids(&rid, connection_id);
    for cid in &other_conns {
        super::dispatcher::send_to_connection_routed(
            &state.ws_storage,
            &state.redis_pubsub,
            cid,
            &msg,
        )
        .await;
    }

    info!(%rid, %playback_id, "Audio playback stopped");
}

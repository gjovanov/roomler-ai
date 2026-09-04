// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use axum::{
    extract::{
        Query, State, WebSocketUpgrade,
        ws::{CloseFrame, Message, WebSocket},
    },
    response::Response,
};
use bson::oid::ObjectId;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct WsParams {
    /// The credential, when it travels in the URL.
    ///
    /// **Optional since the session-cookie path landed.** A browser on our own
    /// origin no longer needs it: the handshake is same-origin, so the
    /// `access_token` cookie is attached automatically and
    /// [`session_cookie`] reads it. Native clients (the agent, the tunnel
    /// CLI) have no cookie jar and always send it.
    ///
    /// Why the browser should stop sending it: a query string is written to
    /// every access log it passes through and kept in browser history, and
    /// long-lived sockets reconnect constantly — so a 7-day credential piled
    /// up in plaintext on disk. `files/nginx-pod.conf` currently answers that
    /// with `access_log off` on `/ws`, which is a stopgap to be removed once
    /// nothing emits `?token=` any more.
    #[serde(default)]
    pub token: Option<String>,
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
    // Wave 2 — the User-Agent and forwarded-for header are read HERE,
    // at the only point where they exist. The UA yields a browser
    // family; the IP resolves to a country and is then dropped (see
    // `user_analytics`) — neither the address nor the raw UA is stored.
    headers: axum::http::HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    match params.role.as_deref() {
        // Native clients. They have no cookie jar, so the URL is the only
        // place their credential can be — and accepting a cookie for these
        // roles would mean a logged-in browser could open an AGENT socket.
        Some("agent") => match params.token {
            Some(t) => ws_upgrade_agent(state, t, params.tid, ws),
            None => unauthorized("agent role requires ?token="),
        },
        Some("tunnel-client") => match params.token {
            Some(t) => ws_upgrade_tunnel_client(state, t, params.tid, ws),
            None => unauthorized("tunnel-client role requires ?token="),
        },
        _ => {
            // Browser. Prefer the query token while older bundles are still
            // cached and still sending it; otherwise the session cookie.
            match params.token {
                Some(t) => ws_upgrade_user(state, t, params.tid, headers, ws).await,
                None => {
                    let Some(t) = session_cookie(&headers) else {
                        return unauthorized("no session");
                    };
                    // ⚠️ A cookie is AMBIENT: the browser attaches it to a
                    // handshake the page never had to prove it could obtain.
                    // A WebSocket upgrade is not subject to CORS, so any page
                    // on the internet may open a socket here — meaning that
                    // once a cookie is accepted, "who is asking" has to be
                    // answered by the request rather than by the credential.
                    //
                    // `SameSite=Lax` already keeps the cookie off a
                    // cross-site handshake, so this is the second lock, not
                    // the only one. It applies ONLY to the cookie path: a
                    // query token is a credential the caller had to get hold
                    // of first, and native clients send no Origin at all.
                    if !origin_is_ours(&state, &headers) {
                        return forbidden("origin not permitted for cookie auth");
                    }
                    ws_upgrade_user(state, t, params.tid, headers, ws).await
                }
            }
        }
    }
}

fn unauthorized(why: &str) -> Response {
    debug!(reason = %why, "ws upgrade refused");
    Response::builder()
        .status(401)
        .body("Unauthorized".into())
        .unwrap()
}

fn forbidden(why: &str) -> Response {
    // The reason stays server-side: it names our own configuration.
    debug!(reason = %why, "ws upgrade refused");
    Response::builder()
        .status(403)
        .body("Forbidden".into())
        .unwrap()
}

/// The `access_token` session cookie, if the browser sent one.
fn session_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    crate::cookies::get(headers, "access_token")
}

/// Is this handshake coming from a page we serve?
///
/// A browser ALWAYS sends `Origin` on a WebSocket handshake, so an absent one
/// means the caller is not a browser — and a non-browser presenting a browser
/// session cookie is not a case worth accommodating. Refuse.
fn origin_is_ours(state: &AppState, headers: &axum::http::HeaderMap) -> bool {
    let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let policy = crate::origin::policy_from_settings(&state.settings);
    crate::origin::is_trusted(&policy, origin)
}

async fn ws_upgrade_user(
    state: AppState,
    token: String,
    tid: Option<String>,
    headers: axum::http::HeaderMap,
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

    // Wave 2 — analytics identity, resolved before the upgrade consumes
    // the headers. `client_ip_from_headers` applies the same trusted-hop
    // rule the rate limiter uses, so a client-forged XFF can't move
    // itself to another country.
    let ua = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let ip = crate::middleware::client_ip::client_ip_from_headers(
        &headers,
        state.settings.app.rate_limit_trusted_hops,
    );
    let analytics_tenant = tid.as_deref().and_then(|t| ObjectId::parse_str(t).ok());

    ws.max_message_size(crate::ws::MAX_WS_MESSAGE_BYTES)
        .max_frame_size(crate::ws::MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            let session =
                crate::user_analytics::open_session(&state, user_id, analytics_tenant, &ua, ip)
                    .await;
            handle_socket(socket, state.clone(), user_id, username, tid).await;
            if let Some(id) = session {
                crate::user_analytics::close_session(&state, id).await;
            }
        })
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

    ws.max_message_size(crate::ws::MAX_WS_MESSAGE_BYTES)
        .max_frame_size(crate::ws::MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
        // Connect-time revocation check. Periodic re-check (every 60 s)
        // lives in `ws::tunnel::handle_tunnel_client_socket`.
        let client = match state
            .network()
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
            tid,
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

    ws.max_message_size(crate::ws::MAX_WS_MESSAGE_BYTES)
        .max_frame_size(crate::ws::MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
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
            // One definition of "this agent may still act", shared with the
            // HTTP ingest routes' `AuthAgent` extractor. This path is the
            // original; the extractor exists because two HTTP routes had
            // silently never grown the equivalent.
            if let Some(reason) = crate::extractors::agent::refusal_reason(&agent) {
                // rc.53: push a `ServerMsg::Goodbye { reason: AgentDeleted }`
                // text frame + a Close frame BEFORE dropping the socket so
                // the agent can log a useful "your row was deleted, re-enrol"
                // line instead of an opaque `ws read` (the failure mode
                // WINHOST-B wedged on for hours pre-rc.53). The agent's
                // `handle_server_msg::ServerMsg::Goodbye` arm decides this is
                // fatal + exits with `AGENT_DELETED_EXIT_CODE = 7`, which
                // the SCM supervisor's rc.53 code-7 fast-alarm fires on the
                // FIRST exit.
                info!(%agent_id, reason, "refusing WS with rc:goodbye");
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
            // FR-69 P5c — the socket is the fleet module's; the host keeps
            // the upgrade and the role gate (D7), the module runs the loop.
            roomler_ai_mod_fleet::socket::handle_agent_socket(
                state.fleet().clone(),
                socket,
                agent_id,
                tenant_id,
                agent.owner_user_id,
                tid,
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

async fn handle_socket(
    socket: WebSocket,
    state: AppState,
    user_id: ObjectId,
    username: String,
    // PR-1 rehome — the (validated) affinity key this conn dialed with,
    // kept for the cross-pod direction rule. None = key-less dial that
    // hashed on client IP.
    dialed_tid: Option<String>,
) {
    let connection_id = Uuid::new_v4().to_string();
    let conn_established_ms = bson::DateTime::now().timestamp_millis();
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
        // `connection_id` lets the client scope its call sessions to this
        // exact WS connection (call/join + call/leave bodies) — the id is
        // minted per socket, so consumers must re-read it after any redial.
        let msg = serde_json::json!({
            "type": "connected",
            "user_id": user_id.to_hex(),
            "connection_id": connection_id,
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
                    dialed_tid.as_deref(),
                    conn_established_ms,
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
    // PR-2 — this conn may have rc sessions proxied on OTHER pods: tell
    // each owner (fire-and-forget; the relay's janitor sweep is the
    // belt), mirroring the C-4 media leave-forwarding below. The `remote`
    // module's (FR-69 P6); a no-op when it is not mounted.
    state.modules.remote_conn_closed(&connection_id);
    state.ws_storage.remove(&user_id, &connection_id, &sender);

    // S6 — drop this pod's online-registry claim once the user's LAST
    // local connection is gone (other pods' claims are theirs to clear).
    if !state.ws_storage.is_connected(&user_id)
        && let Some(pubsub) = &state.redis_pubsub
        && let Err(e) = pubsub.online_remove(&user_id.to_hex()).await
    {
        tracing::debug!("online-registry remove failed: {e}");
    }

    // FR-69 P4 — the modules' per-connection state: conference drops this
    // connection's transports (local, or at the owning pod) and closes its
    // call session — a leave the client never sent.
    let ctx = roomler_core::WsCtx {
        connection_id: connection_id.clone(),
        role: roomler_core::Role::User,
        principal: user_id,
        tenant_id: dialed_tid
            .as_deref()
            .and_then(|t| ObjectId::parse_str(t).ok()),
    };
    state.modules.ws_closed(&ctx).await;

    info!(?user_id, %connection_id, "WebSocket disconnected");
}

#[allow(clippy::too_many_arguments)]
async fn handle_client_message(
    state: &AppState,
    user_id: &ObjectId,
    connection_id: &str,
    username: &str,
    rc_controller_tx: &roomler_ai_remote_control::session::ClientTx,
    text: &str,
    dialed_tid: Option<&str>,
    conn_established_ms: i64,
) {
    // Remote-control messages use a `t` discriminator prefixed with "rc:".
    // Peek at the raw JSON before full parse so we don't pay the cost on
    // every media/presence message. The frame is the `remote` module's
    // (FR-69 P6): its authz gate + dispatch run there, with this
    // connection's Hub-registered sender; `false` = not an rc:* frame, or
    // no `remote` module mounted — either way the arms below get it.
    if text.contains("\"rc:")
        && state
            .modules
            .remote_controller_frame(
                *user_id,
                username,
                rc_controller_tx,
                text,
                dialed_tid,
                conn_established_ms,
                connection_id,
            )
            .await
    {
        return;
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
        _ => {
            // FR-69 — a module's namespace (`typing:*` is chat's). The host
            // knows the socket; the module knows the message.
            if let Some(handler) = state.modules.ws_handler(roomler_core::Role::User, msg_type) {
                let ctx = roomler_core::WsCtx {
                    connection_id: connection_id.to_string(),
                    role: roomler_core::Role::User,
                    principal: *user_id,
                    tenant_id: dialed_tid.and_then(|t| ObjectId::parse_str(t).ok()),
                };
                if let Err(e) = handler.handle(&ctx, parsed.clone()).await {
                    debug!(?user_id, msg_type, %e, "module WS handler failed");
                }
            } else {
                debug!(?user_id, msg_type, "Unknown WS message type");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue, header::COOKIE};

    fn with_cookie(raw: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(COOKIE, HeaderValue::from_str(raw).unwrap());
        h
    }

    #[test]
    fn reads_the_session_cookie_among_others() {
        // Real browsers send several; the session must be found wherever it
        // sits, and a cookie whose name merely ENDS with ours must not match.
        assert_eq!(
            session_cookie(&with_cookie("access_token=abc.def.ghi")).as_deref(),
            Some("abc.def.ghi")
        );
        assert_eq!(
            session_cookie(&with_cookie("theme=dark; access_token=t0k; lang=en")).as_deref(),
            Some("t0k")
        );
        assert_eq!(
            session_cookie(&with_cookie("other_access_token=nope")),
            None,
            "a different cookie ending in our name must not be picked up"
        );
    }

    #[test]
    fn no_cookie_header_and_an_empty_value_both_yield_none() {
        assert_eq!(session_cookie(&HeaderMap::new()), None);
        assert_eq!(session_cookie(&with_cookie("access_token=")), None);
        assert_eq!(session_cookie(&with_cookie("theme=dark")), None);
    }
}

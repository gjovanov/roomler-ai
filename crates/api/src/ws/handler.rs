use axum::{
    extract::{Query, State, WebSocketUpgrade, ws::{Message, WebSocket}},
    response::Response,
};
use bson::oid::ObjectId;
use futures::{SinkExt, StreamExt};
use mediasoup::prelude::*;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct WsParams {
    pub token: String,
}

pub async fn ws_upgrade(
    State(state): State<AppState>,
    Query(params): Query<WsParams>,
    ws: WebSocketUpgrade,
) -> Response {
    // Verify JWT before accepting the WebSocket
    let claims = match state.auth.verify_access_token(&params.token) {
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

    ws.on_upgrade(move |socket| handle_socket(socket, state, user_id))
}

async fn handle_socket(socket: WebSocket, state: AppState, user_id: ObjectId) {
    let connection_id = Uuid::new_v4().to_string();
    info!(?user_id, %connection_id, "WebSocket connected");

    let (sender, mut receiver) = socket.split();
    let sender = Arc::new(Mutex::new(sender));

    // Register connection
    state.ws_storage.add(user_id, connection_id.clone(), sender.clone());

    // Send connected message
    {
        let msg = serde_json::json!({
            "type": "connected",
            "user_id": user_id.to_hex(),
        });
        let mut guard = sender.lock().await;
        let _ = guard.send(Message::text(serde_json::to_string(&msg).unwrap())).await;
    }

    // Message loop
    while let Some(msg) = receiver.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                handle_client_message(&state, &user_id, &connection_id, &text).await;
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

    // Cleanup: remove WS connection
    state.ws_storage.remove(&user_id, &connection_id, &sender);

    // Cleanup: if this connection was in a media room, close their participant and notify peers
    if let Some(conference_id) = state.room_manager.get_connection_conference(&connection_id) {
        // Get remaining connection IDs before closing
        let remaining_conns = state
            .room_manager
            .get_other_connection_ids(&conference_id, &connection_id);

        state
            .room_manager
            .close_participant(&conference_id, &connection_id);

        // Broadcast peer_left to remaining connections
        if !remaining_conns.is_empty() {
            let event = serde_json::json!({
                "type": "media:peer_left",
                "data": {
                    "user_id": user_id.to_hex(),
                    "connection_id": connection_id,
                    "conference_id": conference_id.to_hex(),
                }
            });
            for conn_id in &remaining_conns {
                super::dispatcher::send_to_connection(&state.ws_storage, conn_id, &event).await;
            }
        }
    }

    info!(?user_id, %connection_id, "WebSocket disconnected");
}

async fn handle_client_message(state: &AppState, user_id: &ObjectId, connection_id: &str, text: &str) {
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
            if let Some(channel_id_str) = data.and_then(|d| d.get("channel_id")).and_then(|c| c.as_str()) {
                if let Ok(cid) = ObjectId::parse_str(channel_id_str) {
                    if let Ok(member_ids) = state.channels.find_member_user_ids(cid).await {
                        let recipients: Vec<ObjectId> = member_ids
                            .into_iter()
                            .filter(|id| id != user_id)
                            .collect();
                        let event = serde_json::json!({
                            "type": msg_type,
                            "data": {
                                "channel_id": channel_id_str,
                                "user_id": user_id.to_hex(),
                            }
                        });
                        super::dispatcher::broadcast(&state.ws_storage, &recipients, &event).await;
                    }
                }
            }
        }
        "presence:update" => {
            if let Some(presence) = data.and_then(|d| d.get("presence")).and_then(|p| p.as_str()) {
                let all_users = state.ws_storage.all_user_ids();
                let event = serde_json::json!({
                    "type": "presence:update",
                    "data": {
                        "user_id": user_id.to_hex(),
                        "presence": presence,
                    }
                });
                super::dispatcher::broadcast(&state.ws_storage, &all_users, &event).await;
            }
        }
        // --- Media signaling handlers ---
        "media:join" => {
            handle_media_join(state, user_id, connection_id, data).await;
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
        _ => {
            debug!(?user_id, msg_type, "Unknown WS message type");
        }
    }
}

/// Send a media error message to the user.
async fn send_media_error(state: &AppState, user_id: &ObjectId, message: &str) {
    let msg = serde_json::json!({
        "type": "media:error",
        "data": { "message": message }
    });
    super::dispatcher::send_to_user(&state.ws_storage, user_id, &msg).await;
}

/// Handle media:join — verify room exists, create transports, send capabilities + transports + existing producers
async fn handle_media_join(
    state: &AppState,
    user_id: &ObjectId,
    connection_id: &str,
    data: Option<&serde_json::Value>,
) {
    let conference_id_str = match data.and_then(|d| d.get("conference_id")).and_then(|c| c.as_str()) {
        Some(s) => s,
        None => {
            send_media_error(state, user_id, "Missing conference_id").await;
            return;
        }
    };

    let confid = match ObjectId::parse_str(conference_id_str) {
        Ok(id) => id,
        Err(_) => {
            send_media_error(state, user_id, "Invalid conference_id").await;
            return;
        }
    };

    let room_exists = state.room_manager.has_room(&confid);
    debug!(?user_id, %connection_id, ?confid, room_exists, "media:join room check");
    if !room_exists {
        send_media_error(state, user_id, "Room does not exist").await;
        return;
    }

    // Create transports for this participant (keyed by connection_id so same user
    // can join from multiple tabs without overwriting state)
    let transport_pair = match state
        .room_manager
        .create_transports(confid, *user_id, connection_id.to_string())
        .await
    {
        Ok(tp) => tp,
        Err(e) => {
            send_media_error(state, user_id, &format!("Failed to create transports: {}", e)).await;
            return;
        }
    };

    // Send router capabilities (targeted to this connection only)
    if let Some(room) = state.room_manager.rooms_ref().get(&confid) {
        let caps = serde_json::to_value(room.router.rtp_capabilities()).unwrap_or_default();
        let msg = serde_json::json!({
            "type": "media:router_capabilities",
            "data": { "rtp_capabilities": caps }
        });
        super::dispatcher::send_to_connection(&state.ws_storage, connection_id, &msg).await;
    }

    // Build ICE servers list (TURN) if configured
    let ice_servers: Vec<serde_json::Value> = if let Some(ref url) = state.settings.turn.url {
        vec![serde_json::json!({
            "urls": [url],
            "username": state.settings.turn.username.as_deref().unwrap_or(""),
            "credential": state.settings.turn.password.as_deref().unwrap_or(""),
        })]
    } else {
        vec![]
    };

    let force_relay = state.settings.turn.force_relay.unwrap_or(false);

    if force_relay {
        warn!(
            "force_relay=true with mediasoup — TURN relay won't work because mediasoup \
             doesn't create server-side relay candidates. Set ROOMLER__TURN__FORCE_RELAY=false"
        );
    }

    // Log ICE diagnostics
    info!(
        %connection_id,
        force_relay,
        announced_ip = %state.settings.mediasoup.announced_ip,
        turn_url = ?state.settings.turn.url,
        send_ice_candidates = %transport_pair.send_transport.ice_candidates,
        recv_ice_candidates = %transport_pair.recv_transport.ice_candidates,
        "media:join transport_created ICE diagnostics"
    );

    // Send transport options (targeted to this connection only)
    let msg = serde_json::json!({
        "type": "media:transport_created",
        "data": {
            "send_transport": transport_pair.send_transport,
            "recv_transport": transport_pair.recv_transport,
            "ice_servers": ice_servers,
            "force_relay": force_relay,
        }
    });
    super::dispatcher::send_to_connection(&state.ws_storage, connection_id, &msg).await;

    // Send list of existing producers to the new peer (excludes this connection's own producers)
    let producers = state.room_manager.get_producer_ids(&confid, connection_id);
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
        super::dispatcher::send_to_connection(&state.ws_storage, connection_id, &msg).await;
    }
}

/// Handle media:connect_transport — connect a transport with DTLS parameters
async fn handle_media_connect_transport(
    state: &AppState,
    connection_id: &str,
    data: Option<&serde_json::Value>,
) {
    // We don't need user_id here — the room_manager looks up by connection_id.
    // Errors are logged server-side; no user-facing error for connect_transport.
    let data = match data {
        Some(d) => d,
        None => return,
    };

    let conference_id_str = match data.get("conference_id").and_then(|v| v.as_str()) {
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

    let confid = match ObjectId::parse_str(conference_id_str) {
        Ok(id) => id,
        Err(_) => return,
    };

    if let Err(e) = state
        .room_manager
        .connect_transport(&confid, connection_id, transport_id, dtls_parameters)
        .await
    {
        warn!(%connection_id, %e, "connect_transport failed");
    }
}

/// Handle media:produce — create a producer and broadcast new_producer to peers
async fn handle_media_produce(
    state: &AppState,
    user_id: &ObjectId,
    connection_id: &str,
    data: Option<&serde_json::Value>,
) {
    let data = match data {
        Some(d) => d,
        None => {
            send_media_error(state, user_id, "Missing data").await;
            return;
        }
    };

    let conference_id_str = match data.get("conference_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            send_media_error(state, user_id, "Missing conference_id").await;
            return;
        }
    };
    let kind: MediaKind = match data
        .get("kind")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(k) => k,
        None => {
            send_media_error(state, user_id, "Invalid kind").await;
            return;
        }
    };
    let rtp_parameters: RtpParameters = match data
        .get("rtp_parameters")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(p) => p,
        None => {
            send_media_error(state, user_id, "Invalid rtp_parameters").await;
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

    let confid = match ObjectId::parse_str(conference_id_str) {
        Ok(id) => id,
        Err(_) => {
            send_media_error(state, user_id, "Invalid conference_id").await;
            return;
        }
    };

    match state
        .room_manager
        .produce(&confid, connection_id, kind, rtp_parameters, source.clone())
        .await
    {
        Ok(producer_id) => {
            // Send produce_result to the producing connection only
            let result_msg = serde_json::json!({
                "type": "media:produce_result",
                "data": { "id": producer_id.to_string() }
            });
            super::dispatcher::send_to_connection(&state.ws_storage, connection_id, &result_msg).await;

            // Broadcast new_producer to all other connections (not user_ids, to avoid
            // same-user multi-tab leaking producers back to the producing connection)
            let other_conns = state
                .room_manager
                .get_other_connection_ids(&confid, connection_id);

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
                    super::dispatcher::send_to_connection(&state.ws_storage, conn_id, &event).await;
                }
            }
        }
        Err(e) => {
            send_media_error(state, user_id, &format!("produce failed: {}", e)).await;
        }
    }
}

/// Handle media:consume — create a consumer for a remote producer
async fn handle_media_consume(
    state: &AppState,
    user_id: &ObjectId,
    connection_id: &str,
    data: Option<&serde_json::Value>,
) {
    let data = match data {
        Some(d) => d,
        None => {
            send_media_error(state, user_id, "Missing data").await;
            return;
        }
    };

    let conference_id_str = match data.get("conference_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            send_media_error(state, user_id, "Missing conference_id").await;
            return;
        }
    };
    let producer_id_str = match data.get("producer_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            send_media_error(state, user_id, "Missing producer_id").await;
            return;
        }
    };
    let rtp_capabilities: RtpCapabilities = match data
        .get("rtp_capabilities")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(c) => c,
        None => {
            send_media_error(state, user_id, "Invalid rtp_capabilities").await;
            return;
        }
    };

    let confid = match ObjectId::parse_str(conference_id_str) {
        Ok(id) => id,
        Err(_) => {
            send_media_error(state, user_id, "Invalid conference_id").await;
            return;
        }
    };

    let producer_id = match producer_id_str.parse::<ProducerId>() {
        Ok(id) => id,
        Err(_) => {
            send_media_error(state, user_id, "Invalid producer_id").await;
            return;
        }
    };

    match state
        .room_manager
        .consume(&confid, connection_id, producer_id, &rtp_capabilities)
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
            super::dispatcher::send_to_connection(&state.ws_storage, connection_id, &msg).await;
        }
        Err(e) => {
            send_media_error(state, user_id, &format!("consume failed: {}", e)).await;
        }
    }
}

/// Handle media:producer_close — close a specific producer, notify peers
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

    let conference_id_str = match data.get("conference_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return,
    };
    let producer_id_str = match data.get("producer_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return,
    };

    let confid = match ObjectId::parse_str(conference_id_str) {
        Ok(id) => id,
        Err(_) => return,
    };

    let producer_id = match producer_id_str.parse::<ProducerId>() {
        Ok(id) => id,
        Err(_) => return,
    };

    if state
        .room_manager
        .close_producer(&confid, connection_id, &producer_id)
    {
        // Notify other connections (excluding this connection)
        let other_conns = state
            .room_manager
            .get_other_connection_ids(&confid, connection_id);

        if !other_conns.is_empty() {
            let event = serde_json::json!({
                "type": "media:producer_closed",
                "data": {
                    "producer_id": producer_id.to_string(),
                    "user_id": user_id.to_hex(),
                }
            });
            for conn_id in &other_conns {
                super::dispatcher::send_to_connection(&state.ws_storage, conn_id, &event).await;
            }
        }
    }
}

/// Handle media:leave — close participant media and notify peers
async fn handle_media_leave(
    state: &AppState,
    user_id: &ObjectId,
    connection_id: &str,
    data: Option<&serde_json::Value>,
) {
    let conference_id_str = match data.and_then(|d| d.get("conference_id")).and_then(|c| c.as_str()) {
        Some(s) => s,
        None => return,
    };

    let confid = match ObjectId::parse_str(conference_id_str) {
        Ok(id) => id,
        Err(_) => return,
    };

    // Get remaining connections before closing (excluding this connection)
    let other_conns = state
        .room_manager
        .get_other_connection_ids(&confid, connection_id);

    state
        .room_manager
        .close_participant(&confid, connection_id);

    // Broadcast peer_left to remaining connections
    if !other_conns.is_empty() {
        let event = serde_json::json!({
            "type": "media:peer_left",
            "data": {
                "user_id": user_id.to_hex(),
                "connection_id": connection_id,
                "conference_id": confid.to_hex(),
            }
        });
        for conn_id in &other_conns {
            super::dispatcher::send_to_connection(&state.ws_storage, conn_id, &event).await;
        }
    }
}

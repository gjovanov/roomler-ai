use axum::{
    extract::{Query, State, WebSocketUpgrade, ws::{Message, WebSocket}},
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
}

pub async fn ws_upgrade(
    State(state): State<AppState>,
    Query(params): Query<WsParams>,
    ws: WebSocketUpgrade,
) -> Response {
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

    state.ws_storage.add(user_id, connection_id.clone(), sender.clone());

    {
        let msg = serde_json::json!({
            "type": "connected",
            "user_id": user_id.to_hex(),
        });
        let mut guard = sender.lock().await;
        let _ = guard.send(Message::text(serde_json::to_string(&msg).unwrap())).await;
    }

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

    // Cleanup
    state.ws_storage.remove(&user_id, &connection_id, &sender);

    if let Some(engine) = &state.transcription_engine {
        engine.stop_connection_playbacks(&connection_id);
    }

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
            if let Some(room_id_str) = data.and_then(|d| d.get("room_id")).and_then(|c| c.as_str())
                && let Ok(rid) = ObjectId::parse_str(room_id_str)
                && let Ok(member_ids) = state.rooms.find_member_user_ids(rid).await
            {
                let recipients: Vec<ObjectId> = member_ids
                    .into_iter()
                    .filter(|id| id != user_id)
                    .collect();
                let event = serde_json::json!({
                    "type": msg_type,
                    "data": {
                        "room_id": room_id_str,
                        "user_id": user_id.to_hex(),
                    }
                });
                super::dispatcher::broadcast(&state.ws_storage, &recipients, &event).await;
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
        "media:transcript_toggle" => {
            handle_transcript_toggle(state, user_id, connection_id, data).await;
        }
        "media:play_audio" => {
            handle_play_audio(state, user_id, connection_id, data).await;
        }
        "media:stop_audio" => {
            handle_stop_audio(state, user_id, connection_id, data).await;
        }
        _ => {
            debug!(?user_id, msg_type, "Unknown WS message type");
        }
    }
}

async fn send_media_error(state: &AppState, user_id: &ObjectId, message: &str) {
    let msg = serde_json::json!({
        "type": "media:error",
        "data": { "message": message }
    });
    super::dispatcher::send_to_user(&state.ws_storage, user_id, &msg).await;
}

async fn handle_media_join(
    state: &AppState,
    user_id: &ObjectId,
    connection_id: &str,
    data: Option<&serde_json::Value>,
) {
    let room_id_str = match data.and_then(|d| d.get("room_id")).and_then(|c| c.as_str()) {
        Some(s) => s,
        None => {
            send_media_error(state, user_id, "Missing room_id").await;
            return;
        }
    };

    let rid = match ObjectId::parse_str(room_id_str) {
        Ok(id) => id,
        Err(_) => {
            send_media_error(state, user_id, "Invalid room_id").await;
            return;
        }
    };

    let room_exists = state.room_manager.has_room(&rid);
    debug!(?user_id, %connection_id, ?rid, room_exists, "media:join room check");
    if !room_exists {
        send_media_error(state, user_id, "Room does not exist").await;
        return;
    }

    let transport_pair = match state
        .room_manager
        .create_transports(rid, *user_id, connection_id.to_string())
        .await
    {
        Ok(tp) => tp,
        Err(e) => {
            send_media_error(state, user_id, &format!("Failed to create transports: {}", e)).await;
            return;
        }
    };

    if let Some(room) = state.room_manager.rooms_ref().get(&rid) {
        let caps = serde_json::to_value(room.router.rtp_capabilities()).unwrap_or_default();
        let msg = serde_json::json!({
            "type": "media:router_capabilities",
            "data": { "rtp_capabilities": caps }
        });
        super::dispatcher::send_to_connection(&state.ws_storage, connection_id, &msg).await;
    }

    let ice_servers: Vec<serde_json::Value> = if let Some(ref url) = state.settings.turn.url {
        let (turn_username, turn_credential) = if let Some(ref secret) = state.settings.turn.shared_secret {
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
                state.settings.turn.username.as_deref().unwrap_or("").to_string(),
                state.settings.turn.password.as_deref().unwrap_or("").to_string(),
            )
        };
        // Build TURN URLs with multiple transport variants.
        // UDP TURN often fails behind NAT/firewalls, so include TCP and TLS fallbacks.
        let mut urls: Vec<String> = vec![url.clone()];
        if url.starts_with("turn:") && !url.contains("?transport=") {
            urls.push(format!("{}?transport=tcp", url));
            // Derive TURNS (TLS) URL on port 5349
            let turns_url = url
                .replacen("turn:", "turns:", 1)
                .replace(":3478", ":5349");
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
        info!(
            "force_relay=true — clients will use iceTransportPolicy='relay' via TURN server"
        );
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
    super::dispatcher::send_to_connection(&state.ws_storage, connection_id, &msg).await;

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
        super::dispatcher::send_to_connection(&state.ws_storage, connection_id, &msg).await;
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
            send_media_error(state, user_id, "Missing data").await;
            return;
        }
    };

    let room_id_str = match data.get("room_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            send_media_error(state, user_id, "Missing room_id").await;
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

    let rid = match ObjectId::parse_str(room_id_str) {
        Ok(id) => id,
        Err(_) => {
            send_media_error(state, user_id, "Invalid room_id").await;
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
            super::dispatcher::send_to_connection(&state.ws_storage, connection_id, &result_msg).await;

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
                    super::dispatcher::send_to_connection(&state.ws_storage, conn_id, &event).await;
                }
            }

            if matches!(kind, MediaKind::Audio)
                && let Some(engine) = &state.transcription_engine
                && engine.is_enabled(&rid).await
            {
                start_transcription_pipeline(state, &rid, producer_id, *user_id, connection_id).await;
                spawn_transcript_broadcast(engine.clone(), state.ws_storage.clone(), state.room_manager.clone(), rid);
            }
        }
        Err(e) => {
            send_media_error(state, user_id, &format!("produce failed: {}", e)).await;
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
            send_media_error(state, user_id, "Missing data").await;
            return;
        }
    };

    let room_id_str = match data.get("room_id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            send_media_error(state, user_id, "Missing room_id").await;
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

    let rid = match ObjectId::parse_str(room_id_str) {
        Ok(id) => id,
        Err(_) => {
            send_media_error(state, user_id, "Invalid room_id").await;
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
            super::dispatcher::send_to_connection(&state.ws_storage, connection_id, &msg).await;
        }
        Err(e) => {
            send_media_error(state, user_id, &format!("consume failed: {}", e)).await;
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
        if let Some(engine) = &state.transcription_engine {
            engine.stop_producer(&rid, &producer_id.to_string());
        }
        state.room_manager.remove_rtp_tap(&rid, &producer_id.to_string());

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
                super::dispatcher::send_to_connection(&state.ws_storage, conn_id, &event).await;
            }
        }
    }
}

async fn handle_transcript_toggle(
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
    let enabled = data.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
    let model = data.get("model")
        .and_then(|v| v.as_str())
        .and_then(|s| match s {
            "whisper" | "canary" => Some(s.to_string()),
            _ => None,
        });

    if let Some(ref m) = model {
        info!(%connection_id, %m, "transcript_toggle model requested");
    }

    let rid = match ObjectId::parse_str(room_id_str) {
        Ok(id) => id,
        Err(_) => return,
    };

    if let Some(engine) = &state.transcription_engine {
        if enabled {
            let model_name = model.as_deref().unwrap_or(engine.default_backend_name()).to_string();
            engine.enable_room(rid, model_name).await;

            let all_producers = state.room_manager.get_producer_ids(&rid, "");
            for (uid, conn_id, pid, kind, _source) in all_producers {
                if matches!(kind, MediaKind::Audio) {
                    start_transcription_pipeline(state, &rid, pid, uid, &conn_id).await;
                }
            }

            spawn_transcript_broadcast(engine.clone(), state.ws_storage.clone(), state.room_manager.clone(), rid);
        } else {
            engine.disable_room(rid).await;
        }
    } else {
        warn!("Transcription engine not available — toggling UI state only (no ASR pipeline)");
    }

    let all_conns = state.room_manager.get_other_connection_ids(&rid, connection_id);
    let mut status_data = serde_json::json!({
        "room_id": room_id_str,
        "enabled": enabled,
    });
    if let Some(ref m) = model {
        status_data["model"] = serde_json::json!(m);
    }
    let status_msg = serde_json::json!({
        "type": "media:transcript_status",
        "data": status_data,
    });
    super::dispatcher::send_to_connection(&state.ws_storage, connection_id, &status_msg).await;
    for cid in &all_conns {
        super::dispatcher::send_to_connection(&state.ws_storage, cid, &status_msg).await;
    }
}

async fn start_transcription_pipeline(
    state: &AppState,
    room_id: &ObjectId,
    producer_id: ProducerId,
    user_id: ObjectId,
    _connection_id: &str,
) {
    let engine = match &state.transcription_engine {
        Some(e) => e,
        None => return,
    };

    let speaker_name = match state
        .rooms
        .find_participant_name(*room_id, user_id)
        .await
    {
        Ok(name) => name,
        Err(_) => user_id.to_hex()[..8].to_string(),
    };

    let rtp_rx = match state
        .room_manager
        .create_rtp_tap(room_id, producer_id)
        .await
    {
        Ok(rx) => rx,
        Err(e) => {
            warn!(%e, "Failed to create RTP tap for transcription");
            return;
        }
    };

    engine.start_pipeline(
        *room_id,
        producer_id.to_string(),
        user_id,
        speaker_name,
        rtp_rx,
    );
}

fn spawn_transcript_broadcast(
    engine: Arc<roomler2_transcription::TranscriptionEngine>,
    ws_storage: Arc<super::storage::WsStorage>,
    room_manager: Arc<roomler2_services::media::room_manager::RoomManager>,
    room_id: ObjectId,
) {
    if !engine.try_start_broadcast(room_id) {
        debug!(%room_id, "Broadcast task already active, skipping spawn");
        return;
    }

    let mut rx = engine.subscribe();
    tokio::spawn(async move {
        info!(%room_id, "Transcript broadcast task started");
        while let Ok(event) = rx.recv().await {
            if event.room_id != room_id {
                continue;
            }
            if !engine.is_enabled(&room_id).await {
                break;
            }

            info!(
                text = %event.text,
                speaker = %event.speaker_name,
                "Broadcasting transcript to WS clients"
            );

            let msg = serde_json::json!({
                "type": "media:transcript",
                "data": {
                    "user_id": event.user_id.to_hex(),
                    "speaker_name": event.speaker_name,
                    "text": event.text,
                    "language": event.language,
                    "confidence": event.confidence,
                    "start_time": event.start_time,
                    "end_time": event.end_time,
                    "inference_duration_ms": event.inference_duration_ms,
                    "is_final": event.is_final,
                    "segment_id": event.segment_id,
                }
            });

            let conn_ids = room_manager.get_other_connection_ids(&room_id, "");
            info!(count = conn_ids.len(), "Transcript target connections");
            for cid in &conn_ids {
                super::dispatcher::send_to_connection(&ws_storage, cid, &msg).await;
            }
        }
        engine.clear_broadcast(&room_id);
        info!(%room_id, "Transcript broadcast task exited");
    });
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

    state
        .room_manager
        .close_participant(&rid, connection_id);

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
            super::dispatcher::send_to_connection(&state.ws_storage, conn_id, &event).await;
        }
    }
}

async fn handle_play_audio(
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

    let file = match state.files.base.find_by_id_in_tenant(room.tenant_id, fid).await {
        Ok(f) => f,
        Err(e) => {
            warn!(%e, "Failed to find file for playback");
            return;
        }
    };

    let mut playback_id = String::new();
    if let Some(engine) = &state.transcription_engine {
        if !engine.is_enabled(&rid).await {
            engine.enable_room(rid, engine.default_backend_name().to_string()).await;
        }

        let upload_dir = std::env::var("ROOMLER_UPLOAD_DIR")
            .unwrap_or_else(|_| "/tmp/roomler2-uploads".to_string());
        let file_path = format!("{}/{}", upload_dir, file.storage_key);

        let speaker_name = match state
            .rooms
            .find_participant_name(rid, *user_id)
            .await
        {
            Ok(name) => name,
            Err(_) => user_id.to_hex()[..8].to_string(),
        };

        if let Some(pid) = engine
            .start_file_playback(rid, file_path.clone(), *user_id, speaker_name)
            .await
        {
            playback_id = pid.clone();
            engine.track_playback(connection_id, &pid);
            info!(%pid, %file_path, "File playback pipeline started");
        } else {
            warn!(%file_path, "start_file_playback returned None — no ASR backend?");
        }

        spawn_transcript_broadcast(engine.clone(), state.ws_storage.clone(), state.room_manager.clone(), rid);
    } else {
        warn!("Transcription engine unavailable — file plays audio-only, no transcript");
    }

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

    super::dispatcher::send_to_connection(&state.ws_storage, connection_id, &msg).await;
    let other_conns = state.room_manager.get_other_connection_ids(&rid, connection_id);
    for cid in &other_conns {
        super::dispatcher::send_to_connection(&state.ws_storage, cid, &msg).await;
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

    if let Some(engine) = &state.transcription_engine {
        engine.stop_pipeline(playback_id);
    }

    let msg = serde_json::json!({
        "type": "media:audio_playback",
        "data": {
            "action": "stop",
            "playback_id": playback_id,
            "room_id": room_id_str,
        }
    });

    super::dispatcher::send_to_connection(&state.ws_storage, connection_id, &msg).await;
    let other_conns = state.room_manager.get_other_connection_ids(&rid, connection_id);
    for cid in &other_conns {
        super::dispatcher::send_to_connection(&state.ws_storage, cid, &msg).await;
    }

    info!(%rid, %playback_id, "Audio playback stopped");
}

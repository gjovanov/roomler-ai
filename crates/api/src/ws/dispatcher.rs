use axum::extract::ws::Message;
use bson::oid::ObjectId;
use futures::SinkExt;
use std::sync::Arc;
use tracing::{debug, warn};

use super::redis_pubsub::RedisPubSub;
use super::storage::WsStorage;

/// Broadcasts a JSON message to all connections of the specified users.
pub async fn broadcast(ws_storage: &WsStorage, user_ids: &[ObjectId], message: &serde_json::Value) {
    let text = serde_json::to_string(message).unwrap_or_default();

    for user_id in user_ids {
        let senders = ws_storage.get_senders(user_id);
        for sender in senders {
            let text = text.clone();
            let mut guard = sender.lock().await;
            if let Err(e) = guard.send(Message::text(text)).await {
                warn!(?user_id, %e, "Failed to send WS message");
            } else {
                debug!(?user_id, "WS message sent");
            }
        }
    }
}

/// Sends a JSON message to a specific user's connections.
pub async fn send_to_user(ws_storage: &WsStorage, user_id: &ObjectId, message: &serde_json::Value) {
    broadcast(ws_storage, &[*user_id], message).await;
}

/// Broadcasts a JSON message locally AND publishes to Redis for cross-instance delivery.
/// Use this for events that must reach users on any server instance (e.g., message:create,
/// typing, presence, reactions, call events).
pub async fn broadcast_with_redis(
    ws_storage: &WsStorage,
    redis_pubsub: &Option<Arc<RedisPubSub>>,
    user_ids: &[ObjectId],
    message: &serde_json::Value,
) {
    // Local broadcast (same instance)
    broadcast(ws_storage, user_ids, message).await;

    // Cross-instance broadcast via Redis Pub/Sub. `origin` lets the
    // subscriber loop drop envelopes this instance published itself —
    // local delivery already happened above, so without the guard every
    // event double-delivers on its origin pod.
    if let Some(pubsub) = redis_pubsub {
        let envelope = serde_json::json!({
            "origin": pubsub.instance_id(),
            "user_ids": user_ids.iter().map(|id| id.to_hex()).collect::<Vec<_>>(),
            "message": message,
        });
        if let Err(e) = pubsub.publish(&envelope.to_string()).await {
            tracing::error!("Failed to publish to Redis Pub/Sub: {}", e);
        }
    }
}

/// S6 — broadcast to EVERY connected user on EVERY instance.
///
/// `broadcast_with_redis` ships an explicit recipient list computed by
/// the publisher; for presence fan-out that list was the PUBLISHING
/// pod's `all_user_ids()`, so users whose sockets live on the other pod
/// never heard about it. This variant marks the envelope
/// `"broadcast": true` — each subscriber delivers to its OWN local
/// user set instead of the origin's snapshot.
pub async fn broadcast_all_with_redis(
    ws_storage: &WsStorage,
    redis_pubsub: &Option<Arc<RedisPubSub>>,
    message: &serde_json::Value,
) {
    let local_users = ws_storage.all_user_ids();
    broadcast(ws_storage, &local_users, message).await;

    if let Some(pubsub) = redis_pubsub {
        let envelope = serde_json::json!({
            "origin": pubsub.instance_id(),
            "broadcast": true,
            "message": message,
        });
        if let Err(e) = pubsub.publish(&envelope.to_string()).await {
            tracing::error!("Failed to publish broadcast to Redis Pub/Sub: {}", e);
        }
    }
}

/// Sends a JSON message to a specific user locally AND via Redis for cross-instance delivery.
pub async fn send_to_user_with_redis(
    ws_storage: &WsStorage,
    redis_pubsub: &Option<Arc<RedisPubSub>>,
    user_id: &ObjectId,
    message: &serde_json::Value,
) {
    broadcast_with_redis(ws_storage, redis_pubsub, &[*user_id], message).await;
}

/// Sends a JSON message to a specific connection by connection_id.
/// Used for media signaling responses that should target a single tab/device.
pub async fn send_to_connection(
    ws_storage: &WsStorage,
    connection_id: &str,
    message: &serde_json::Value,
) {
    if let Some(sender) = ws_storage.get_sender_by_connection(connection_id) {
        let text = serde_json::to_string(message).unwrap_or_default();
        let mut guard = sender.lock().await;
        if let Err(e) = guard.send(Message::text(text)).await {
            warn!(%connection_id, %e, "Failed to send WS message to connection");
        }
    }
}

/// C-4 — cluster-aware single-connection delivery: local socket when
/// present, else a **conn-addressed** envelope on the global channel
/// (connection UUIDs are globally unique, so no per-conn directory is
/// needed — the pod holding the socket delivers, everyone else drops).
/// This is how a media-room owner reaches participants whose WS lives
/// on another pod.
pub async fn send_to_connection_routed(
    ws_storage: &WsStorage,
    redis_pubsub: &Option<Arc<RedisPubSub>>,
    connection_id: &str,
    message: &serde_json::Value,
) {
    if ws_storage.get_sender_by_connection(connection_id).is_some() {
        send_to_connection(ws_storage, connection_id, message).await;
        return;
    }
    if let Some(pubsub) = redis_pubsub {
        let envelope = serde_json::json!({
            "origin": pubsub.instance_id(),
            "conn": connection_id,
            "message": message,
        });
        if let Err(e) = pubsub.publish(&envelope.to_string()).await {
            tracing::error!("Failed to publish conn-addressed WS message: {}", e);
        }
    }
}

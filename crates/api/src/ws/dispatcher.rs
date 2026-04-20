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

    // Cross-instance broadcast via Redis Pub/Sub
    if let Some(pubsub) = redis_pubsub {
        let envelope = serde_json::json!({
            "user_ids": user_ids.iter().map(|id| id.to_hex()).collect::<Vec<_>>(),
            "message": message,
        });
        if let Err(e) = pubsub.publish(&envelope.to_string()).await {
            tracing::error!("Failed to publish to Redis Pub/Sub: {}", e);
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

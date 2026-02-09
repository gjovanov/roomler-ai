use bson::oid::ObjectId;
use dashmap::DashMap;
use futures::stream::SplitSink;
use axum::extract::ws::{Message, WebSocket};
use std::sync::Arc;
use tokio::sync::Mutex;

pub type WsSender = Arc<Mutex<SplitSink<WebSocket, Message>>>;

/// Tracks all active WebSocket connections by user ID and connection ID.
/// Each user can have multiple connections (multiple tabs/devices).
pub struct WsStorage {
    /// user_id -> Vec of senders (for user-level broadcasts)
    connections: DashMap<ObjectId, Vec<WsSender>>,
    /// connection_id -> (user_id, sender) for connection-targeted sends
    connection_map: DashMap<String, (ObjectId, WsSender)>,
}

impl WsStorage {
    pub fn new() -> Self {
        Self {
            connections: DashMap::new(),
            connection_map: DashMap::new(),
        }
    }

    pub fn add(&self, user_id: ObjectId, connection_id: String, sender: WsSender) {
        self.connections
            .entry(user_id)
            .or_default()
            .push(sender.clone());
        self.connection_map
            .insert(connection_id, (user_id, sender));
    }

    pub fn remove(&self, user_id: &ObjectId, connection_id: &str, sender: &WsSender) {
        if let Some(mut senders) = self.connections.get_mut(user_id) {
            senders.retain(|s| !Arc::ptr_eq(s, sender));
            if senders.is_empty() {
                drop(senders);
                self.connections.remove(user_id);
            }
        }
        self.connection_map.remove(connection_id);
    }

    pub fn get_senders(&self, user_id: &ObjectId) -> Vec<WsSender> {
        self.connections
            .get(user_id)
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    /// Get the sender for a specific connection ID.
    pub fn get_sender_by_connection(&self, connection_id: &str) -> Option<WsSender> {
        self.connection_map
            .get(connection_id)
            .map(|entry| entry.value().1.clone())
    }

    pub fn all_user_ids(&self) -> Vec<ObjectId> {
        self.connections.iter().map(|r| *r.key()).collect()
    }

    pub fn connection_count(&self) -> usize {
        self.connections
            .iter()
            .map(|r| r.value().len())
            .sum()
    }
}

impl Default for WsStorage {
    fn default() -> Self {
        Self::new()
    }
}

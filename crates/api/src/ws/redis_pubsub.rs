use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use redis::aio::ConnectionManager;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

const CHANNEL_NAME: &str = "roomler:ws";

/// TTL on `roomler:online:<uid>` keys. Refreshed by each pod's 30 s
/// heartbeat while the user has local connections — 3× headroom so a
/// single missed beat (GC pause, Redis blip) doesn't flap presence.
const ONLINE_TTL_SECS: u64 = 90;

/// Manages Redis Pub/Sub for cross-instance WebSocket event distribution.
///
/// Each application instance publishes WS events to a shared Redis channel.
/// A background subscriber task receives messages from other instances and
/// forwards them to local WebSocket connections via a broadcast channel.
///
/// Every published envelope carries this instance's `instance_id`; the
/// subscriber loop in `main.rs` drops envelopes whose origin matches its own
/// id. Without that guard the publishing pod's own subscription re-delivers
/// every event locally (local delivery already happened in
/// `dispatcher::broadcast_with_redis`) — the "double notification" bug.
#[derive(Clone)]
pub struct RedisPubSub {
    publisher: ConnectionManager,
    channel: String,
    instance_id: String,
}

impl RedisPubSub {
    pub async fn new(redis_url: &str) -> Result<Self, redis::RedisError> {
        let client = redis::Client::open(redis_url)?;
        let publisher = ConnectionManager::new(client).await?;
        info!("Redis Pub/Sub publisher connected to {}", redis_url);
        Ok(Self {
            publisher,
            channel: CHANNEL_NAME.to_string(),
            instance_id: uuid::Uuid::new_v4().to_string(),
        })
    }

    /// Per-process origin id stamped on every published envelope.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Publish a message to Redis for other instances to receive.
    pub async fn publish(&self, message: &str) -> Result<(), redis::RedisError> {
        let mut conn = self.publisher.clone();
        redis::cmd("PUBLISH")
            .arg(&self.channel)
            .arg(message)
            .query_async::<()>(&mut conn)
            .await?;
        Ok(())
    }

    /// Round-trip a PING on the publisher connection — the `/health/ready`
    /// dependency check.
    pub async fn ping(&self) -> Result<(), redis::RedisError> {
        let mut conn = self.publisher.clone();
        redis::cmd("PING").query_async::<String>(&mut conn).await?;
        Ok(())
    }

    // ── S6 cross-pod online registry ────────────────────────────────
    //
    // `WsStorage::is_connected` only sees THIS pod's sockets. With two
    // pods, the offline-dedupe in `routes/helpers.rs` would push+email
    // every user whose WS lives on the other pod. Each pod mirrors its
    // local user set into Redis: one SET per user
    // (`roomler:online:<uid>`) whose members are instance ids, with a
    // key TTL refreshed by a 30 s heartbeat — so a crashed pod's
    // entries age out within `ONLINE_TTL_SECS` instead of leaking
    // forever. The registry is advisory (dedupe/UX), not authoritative
    // presence: a ≤90 s stale window on pod crash is acceptable.

    fn online_key(user_id_hex: &str) -> String {
        format!("roomler:online:{user_id_hex}")
    }

    /// Mark a user online from this instance (idempotent) and refresh
    /// the key's TTL.
    pub async fn online_add(&self, user_id_hex: &str) -> Result<(), redis::RedisError> {
        let mut conn = self.publisher.clone();
        let key = Self::online_key(user_id_hex);
        redis::pipe()
            .cmd("SADD")
            .arg(&key)
            .arg(&self.instance_id)
            .ignore()
            .cmd("EXPIRE")
            .arg(&key)
            .arg(ONLINE_TTL_SECS)
            .ignore()
            .query_async::<()>(&mut conn)
            .await
    }

    /// Remove this instance's membership when the user's LAST local
    /// connection closes. Redis drops the set key automatically once
    /// its final member is removed.
    pub async fn online_remove(&self, user_id_hex: &str) -> Result<(), redis::RedisError> {
        let mut conn = self.publisher.clone();
        redis::cmd("SREM")
            .arg(Self::online_key(user_id_hex))
            .arg(&self.instance_id)
            .query_async::<()>(&mut conn)
            .await
    }

    /// Whether ANY instance currently claims this user online.
    pub async fn online_anywhere(&self, user_id_hex: &str) -> Result<bool, redis::RedisError> {
        let mut conn = self.publisher.clone();
        let n: i64 = redis::cmd("EXISTS")
            .arg(Self::online_key(user_id_hex))
            .query_async(&mut conn)
            .await?;
        Ok(n > 0)
    }

    /// Spawn the subscriber task with reconnect-and-backoff. Before S0 a
    /// dropped Redis connection ended the subscription permanently (one
    /// error log, then silent local-only delivery forever). `alive` mirrors
    /// whether a subscription is currently established — read by
    /// `/health/ready`.
    pub fn subscribe_with_reconnect(
        redis_url: String,
        tx: broadcast::Sender<String>,
        alive: Arc<AtomicBool>,
    ) {
        tokio::spawn(async move {
            let mut backoff_secs = 1u64;
            loop {
                match Self::run_subscription(&redis_url, &tx, &alive).await {
                    Ok(()) => {
                        // A live subscription ended (Redis restart / network
                        // blip) — retry promptly.
                        backoff_secs = 1;
                        warn!("Redis Pub/Sub subscription stream ended; reconnecting");
                    }
                    Err(e) => {
                        warn!("Redis Pub/Sub subscribe failed: {e}; retrying in {backoff_secs}s");
                    }
                }
                alive.store(false, Ordering::Relaxed);
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(30);
            }
        });
    }

    /// One subscription lifetime: connect, subscribe, pump messages until the
    /// stream ends. `Ok(())` = the subscription was established and later
    /// ended; `Err` = it never got established.
    async fn run_subscription(
        redis_url: &str,
        tx: &broadcast::Sender<String>,
        alive: &AtomicBool,
    ) -> Result<(), redis::RedisError> {
        let client = redis::Client::open(redis_url)?;
        let mut pubsub = client.get_async_pubsub().await?;
        pubsub.subscribe(CHANNEL_NAME).await?;
        alive.store(true, Ordering::Relaxed);
        info!("Redis Pub/Sub subscribed to channel: {}", CHANNEL_NAME);

        use futures::StreamExt;
        let mut stream = pubsub.on_message();
        while let Some(msg) = stream.next().await {
            match msg.get_payload::<String>() {
                Ok(payload) => {
                    // broadcast::send only fails if there are no receivers,
                    // which is fine — it means no one is listening yet.
                    let _ = tx.send(payload);
                }
                Err(e) => {
                    error!("Failed to decode Redis Pub/Sub payload: {}", e);
                }
            }
        }
        Ok(())
    }
}

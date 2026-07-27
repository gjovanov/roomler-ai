use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use redis::aio::ConnectionManager;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

const CHANNEL_NAME: &str = "roomler:ws";

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

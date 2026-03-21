use redis::aio::ConnectionManager;
use tokio::sync::broadcast;
use tracing::{error, info};

const CHANNEL_NAME: &str = "roomler:ws";

/// Manages Redis Pub/Sub for cross-instance WebSocket event distribution.
///
/// Each application instance publishes WS events to a shared Redis channel.
/// A background subscriber task receives messages from other instances and
/// forwards them to local WebSocket connections via a broadcast channel.
#[derive(Clone)]
pub struct RedisPubSub {
    publisher: ConnectionManager,
    channel: String,
}

impl RedisPubSub {
    pub async fn new(redis_url: &str) -> Result<Self, redis::RedisError> {
        let client = redis::Client::open(redis_url)?;
        let publisher = ConnectionManager::new(client).await?;
        info!("Redis Pub/Sub publisher connected to {}", redis_url);
        Ok(Self {
            publisher,
            channel: CHANNEL_NAME.to_string(),
        })
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

    /// Start a subscriber that listens on the Redis channel and forwards
    /// messages into a tokio broadcast channel. Returns immediately after
    /// spawning the background listener task.
    pub async fn subscribe(
        redis_url: &str,
        tx: broadcast::Sender<String>,
    ) -> Result<(), redis::RedisError> {
        let client = redis::Client::open(redis_url)?;
        let mut pubsub = client.get_async_pubsub().await?;
        pubsub.subscribe(CHANNEL_NAME).await?;
        info!("Redis Pub/Sub subscribed to channel: {}", CHANNEL_NAME);

        tokio::spawn(async move {
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
            error!("Redis Pub/Sub subscription stream ended unexpectedly");
        });

        Ok(())
    }
}

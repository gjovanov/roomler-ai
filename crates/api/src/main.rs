use bson::oid::ObjectId;
use roomler2_api::{build_router, state::AppState, ws::{dispatcher, redis_pubsub::RedisPubSub}};
use roomler2_config::Settings;
use roomler2_db::{connect, indexes::ensure_indexes};
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env file (silently ignore if missing)
    dotenvy::dotenv().ok();

    // Initialize tracing
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            "roomler2_api=debug,roomler2_services=debug,roomler2_db=debug,tower_http=debug"
                .into()
        }))
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Load config
    let settings = Settings::load()?;
    info!("Starting Roomler2 API on {}:{}", settings.app.host, settings.app.port);
    info!(
        listen_ip = %settings.mediasoup.listen_ip,
        announced_ip = %settings.mediasoup.announced_ip,
        rtc_ports = %format!("{}-{}", settings.mediasoup.rtc_min_port, settings.mediasoup.rtc_max_port),
        turn_url = ?settings.turn.url,
        force_relay = ?settings.turn.force_relay,
        "Mediasoup/TURN config"
    );

    // Connect to MongoDB
    let db = connect(&settings).await?;

    // Ensure indexes
    ensure_indexes(&db).await?;

    // Build app state (async: spawns mediasoup workers)
    let app_state = AppState::new(db.clone(), settings.clone()).await?;

    // Clean up stale calls (in_progress with 0 participants) left over from previous runs
    {
        let rooms_coll = db.collection::<bson::Document>("rooms");
        let result = rooms_coll
            .update_many(
                bson::doc! { "conference_status": "in_progress", "participant_count": { "$lte": 0 } },
                bson::doc! { "$set": { "conference_status": "ended" } },
            )
            .await
            .ok();
        if let Some(res) = result
            && res.modified_count > 0
        {
            info!("Cleaned up {} stale calls (in_progress with 0 participants)", res.modified_count);
        }
    }

    // Start Redis Pub/Sub subscriber for cross-instance WS delivery
    if app_state.redis_pubsub.is_some() {
        let (redis_tx, _) = tokio::sync::broadcast::channel::<String>(1024);
        let ws_storage = app_state.ws_storage.clone();
        let mut redis_rx = redis_tx.subscribe();

        // Start the Redis subscriber (spawns a background task internally)
        if let Err(e) = RedisPubSub::subscribe(&settings.redis.url, redis_tx).await {
            error!("Failed to start Redis Pub/Sub subscriber: {}", e);
        } else {
            // Forward Redis messages to local WS connections
            tokio::spawn(async move {
                while let Ok(payload) = redis_rx.recv().await {
                    if let Ok(envelope) = serde_json::from_str::<serde_json::Value>(&payload)
                        && let (Some(user_ids_val), Some(message)) = (
                            envelope["user_ids"].as_array(),
                            envelope.get("message"),
                        )
                    {
                        let ids: Vec<ObjectId> = user_ids_val
                            .iter()
                            .filter_map(|v| v.as_str().and_then(|s| ObjectId::parse_str(s).ok()))
                            .collect();
                        // Deliver to local connections only (no re-publish to Redis)
                        dispatcher::broadcast(&ws_storage, &ids, message).await;
                    }
                }
                error!("Redis Pub/Sub forwarding task ended unexpectedly");
            });
            info!("Redis Pub/Sub cross-instance WS delivery enabled");
        }
    }

    // Build router
    let app = build_router(app_state);

    // Start server
    let addr = format!("{}:{}", settings.app.host, settings.app.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("Listening on {}", addr);

    axum::serve(listener, app).await?;

    Ok(())
}

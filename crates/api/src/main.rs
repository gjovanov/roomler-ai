use bson::oid::ObjectId;
use roomler_ai_api::{
    build_router,
    state::AppState,
    ws::{dispatcher, redis_pubsub::RedisPubSub},
};
use roomler_ai_config::Settings;
use roomler_ai_db::{connect, indexes::ensure_indexes};
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env file (silently ignore if missing)
    dotenvy::dotenv().ok();

    // Initialize tracing
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            "roomler_ai_api=debug,roomler_ai_services=debug,roomler_ai_db=debug,tower_http=debug"
                .into()
        }))
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Load config
    let settings = Settings::load()?;
    info!(
        "Starting Roomler2 API on {}:{}",
        settings.app.host, settings.app.port
    );
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

    // Clean up ALL stale calls — no calls can be active at server startup
    {
        let rooms_coll = db.collection::<bson::Document>("rooms");
        let result = rooms_coll
            .update_many(
                bson::doc! { "conference_status": "in_progress" },
                bson::doc! { "$set": { "conference_status": "ended", "participant_count": 0_i32 } },
            )
            .await
            .ok();
        if let Some(res) = result
            && res.modified_count > 0
        {
            info!(
                "Cleaned up {} stale calls (all in_progress reset to ended)",
                res.modified_count
            );
        }
    }

    // Fix thread metadata for existing thread roots with null metadata
    // (bug: MongoDB $inc fails on null subdocuments, so reply_count was never set)
    {
        let msgs_coll = db.collection::<bson::Document>("messages");
        // Count replies per thread parent and rebuild metadata
        use futures::TryStreamExt;
        let pipeline = vec![
            bson::doc! { "$match": { "thread_id": { "$ne": null } } },
            bson::doc! { "$group": {
                "_id": "$thread_id",
                "reply_count": { "$sum": 1 },
                "last_reply_at": { "$max": "$created_at" },
                "last_reply_user_id": { "$last": "$author_id" },
                "participant_ids": { "$addToSet": "$author_id" },
            }},
        ];
        if let Ok(mut cursor) = msgs_coll.aggregate(pipeline).await {
            let mut fixed = 0u64;
            while let Ok(Some(doc)) = cursor.try_next().await {
                if let (Some(parent_id), Some(count)) = (
                    doc.get_object_id("_id").ok(),
                    doc.get_i32("reply_count").ok(),
                ) {
                    let update = bson::doc! {
                        "$set": {
                            "is_thread_root": true,
                            "thread_metadata": {
                                "reply_count": count,
                                "last_reply_at": doc.get("last_reply_at"),
                                "last_reply_user_id": doc.get("last_reply_user_id"),
                                "participant_ids": doc.get("participant_ids"),
                            },
                        },
                    };
                    if msgs_coll
                        .update_one(bson::doc! { "_id": parent_id }, update)
                        .await
                        .is_ok()
                    {
                        fixed += 1;
                    }
                }
            }
            if fixed > 0 {
                info!(
                    "Rebuilt thread metadata for {} thread parent messages",
                    fixed
                );
            }
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
                        && let (Some(user_ids_val), Some(message)) =
                            (envelope["user_ids"].as_array(), envelope.get("message"))
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

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

    // Loud, non-fatal warning if the JWT secret is still the built-in
    // default ("change-me-in-production"). The default is fine for
    // `cargo run` against localhost, but any reachable deployment must
    // override `ROOMLER__JWT__SECRET` — otherwise any user can forge
    // tokens. Hard-fail gated on an explicit `app.environment=production`
    // setting is a follow-up; warning here matches the existing posture
    // in CLAUDE.md "Known Issues".
    if settings.jwt.secret == "change-me-in-production" {
        error!(
            "JWT secret is the built-in default \"change-me-in-production\" — \
             set ROOMLER__JWT__SECRET before exposing this server."
        );
    }

    // Stripe half-configuration is a silent checkout-killer: with a secret
    // key but empty price ids, every valid-plan checkout errors (prod hit
    // exactly this — empty ROOMLER__STRIPE__PRICE_PRO/BUSINESS in the
    // configmap). Loud at startup; the checkout path also refuses cleanly
    // (StripeError::PriceNotConfigured).
    if !settings.stripe.secret_key.is_empty()
        && (settings.stripe.price_pro.is_empty() || settings.stripe.price_business.is_empty())
    {
        error!(
            "Stripe secret key is set but price_pro/price_business is empty — \
             checkout WILL fail; set ROOMLER__STRIPE__PRICE_PRO and \
             ROOMLER__STRIPE__PRICE_BUSINESS."
        );
    }

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

    // S6 — leader-gate the startup maintenance below. With two pods, a
    // restarting pod must NOT reset `in_progress` calls that are live on
    // the OTHER pod's mediasoup. A short Mongo lease elects exactly one
    // maintenance runner per window: the filtered upsert succeeds for one
    // pod (fresh insert, or expired-lease takeover) and fails with a
    // duplicate-key / zero-match for everyone else. A skipped reset is
    // healed by the media:join get-or-create belt in ws/handler.rs.
    let startup_leader = {
        let locks = db.collection::<bson::Document>("locks");
        let now = bson::DateTime::now();
        let lease_until = bson::DateTime::from_millis(now.timestamp_millis() + 120_000);
        match locks
            .update_one(
                bson::doc! { "_id": "startup_maintenance", "expires_at": { "$lt": now } },
                bson::doc! { "$set": { "expires_at": lease_until } },
            )
            .upsert(true)
            .await
        {
            Ok(r) => r.modified_count > 0 || r.upserted_id.is_some(),
            // Concurrent upsert loser (E11000 on _id) or Mongo hiccup —
            // either way, someone else owns maintenance this window.
            Err(_) => false,
        }
    };
    if !startup_leader {
        info!(
            "Startup maintenance lease held elsewhere — skipping stale-call reset + thread migration"
        );
    }

    // Clean up ALL stale calls — no calls can be active at server startup
    if startup_leader {
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
    if startup_leader {
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

    // One-off migration: a pre-S0 pod wrote uploads to the (ephemeral) local
    // dir; when the S3 backend is enabled, sweep whatever survived up to S3
    // so those file records resolve again. No-op on the local backend.
    {
        let storage = app_state.storage.clone();
        tokio::spawn(async move { storage.migrate_local_to_s3().await });
    }

    // Start Redis Pub/Sub subscriber for cross-instance WS delivery
    if let Some(pubsub) = &app_state.redis_pubsub {
        let (redis_tx, _) = tokio::sync::broadcast::channel::<String>(1024);
        let ws_storage = app_state.ws_storage.clone();
        let mut redis_rx = redis_tx.subscribe();
        let own_instance = pubsub.instance_id().to_string();

        // Subscriber with reconnect-and-backoff; flips `redis_sub_alive`
        // for `/health/ready`.
        RedisPubSub::subscribe_with_reconnect(
            settings.redis.url.clone(),
            redis_tx,
            app_state.redis_sub_alive.clone(),
        );

        // Forward Redis messages to local WS connections
        tokio::spawn(async move {
            loop {
                match redis_rx.recv().await {
                    Ok(payload) => {
                        let Ok(envelope) = serde_json::from_str::<serde_json::Value>(&payload)
                        else {
                            continue;
                        };
                        // Self-echo guard: this instance already delivered
                        // locally at publish time — re-delivering here
                        // doubles every event on its origin pod.
                        if envelope["origin"].as_str() == Some(own_instance.as_str()) {
                            continue;
                        }
                        // S6 broadcast envelopes (presence fan-out): the
                        // publisher can't know OUR user set, so it flags
                        // the envelope and each pod delivers to its own
                        // local connections.
                        if envelope["broadcast"].as_bool() == Some(true) {
                            if let Some(message) = envelope.get("message") {
                                let ids = ws_storage.all_user_ids();
                                dispatcher::broadcast(&ws_storage, &ids, message).await;
                            }
                            continue;
                        }
                        if let (Some(user_ids_val), Some(message)) =
                            (envelope["user_ids"].as_array(), envelope.get("message"))
                        {
                            let ids: Vec<ObjectId> = user_ids_val
                                .iter()
                                .filter_map(|v| {
                                    v.as_str().and_then(|s| ObjectId::parse_str(s).ok())
                                })
                                .collect();
                            // Deliver to local connections only (no re-publish to Redis)
                            dispatcher::broadcast(&ws_storage, &ids, message).await;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        // Slow consumer: events were dropped, keep forwarding
                        // (the old `while let Ok` silently KILLED forwarding
                        // on the first lag).
                        tracing::warn!("Redis Pub/Sub forwarder lagged; dropped {n} events");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            error!("Redis Pub/Sub forwarding task ended (channel closed)");
        });
        info!("Redis Pub/Sub cross-instance WS delivery enabled");

        // S6 — online-registry heartbeat: re-assert this pod's local
        // users in Redis every 30 s (TTL 90 s). Heals missed SREMs on
        // abnormal disconnects and lets a crashed pod's claims age out.
        let hb_pubsub = pubsub.clone();
        let hb_storage = app_state.ws_storage.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                for uid in hb_storage.all_user_ids() {
                    if let Err(e) = hb_pubsub.online_add(&uid.to_hex()).await {
                        tracing::debug!("online-registry heartbeat failed: {e}");
                        break; // Redis down — retry whole set next tick.
                    }
                }
            }
        });
    }

    // Build router
    let app = build_router(app_state);

    // Start server
    let addr = format!("{}:{}", settings.app.host, settings.app.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("Listening on {}", addr);

    // `with_connect_info` so the rate limiter has a peer address to fall back
    // on when `X-Forwarded-For` is absent or too short to trust (a request
    // that did not come through our proxy chain).
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;

    Ok(())
}

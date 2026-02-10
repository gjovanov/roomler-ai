use roomler2_api::{build_router, state::AppState};
use roomler2_config::Settings;
use roomler2_db::{connect, indexes::ensure_indexes};
use tracing::info;
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
    let app_state = AppState::new(db, settings.clone()).await?;

    // Build router
    let app = build_router(app_state);

    // Start server
    let addr = format!("{}:{}", settings.app.host, settings.app.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("Listening on {}", addr);

    axum::serve(listener, app).await?;

    Ok(())
}

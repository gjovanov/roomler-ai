pub mod error;
pub mod extractors;
pub mod middleware;
pub mod routes;
pub mod state;
pub mod ws;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{delete, get, post, put},
};
use state::AppState;
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};

pub fn build_router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Auth routes (no tenant prefix)
    let auth_routes = Router::new()
        .route("/register", post(routes::auth::register))
        .route("/login", post(routes::auth::login))
        .route("/logout", post(routes::auth::logout))
        .route("/refresh", post(routes::auth::refresh))
        .route("/me", get(routes::auth::me))
        .route("/me", put(routes::auth::me));

    // Tenant routes
    let tenant_routes = Router::new()
        .route("/", get(routes::tenant::list))
        .route("/", post(routes::tenant::create))
        .route("/{tenant_id}", get(routes::tenant::get));

    // Member routes (under tenant)
    let member_routes = Router::new()
        .route("/", get(routes::user::list_members).post(routes::invite::add_member));

    // Room routes (under tenant) — replaces channel + conference
    let room_routes = Router::new()
        .route("/", get(routes::room::list))
        .route("/", post(routes::room::create))
        .route("/explore", get(routes::room::explore))
        .route("/{room_id}", get(routes::room::get))
        .route("/{room_id}", put(routes::room::update))
        .route("/{room_id}", delete(routes::room::delete))
        .route("/{room_id}/join", post(routes::room::join))
        .route("/{room_id}/leave", post(routes::room::leave))
        .route("/{room_id}/member", get(routes::room::members))
        // Call endpoints
        .route("/{room_id}/call/start", post(routes::room::call_start))
        .route("/{room_id}/call/join", post(routes::room::call_join))
        .route("/{room_id}/call/leave", post(routes::room::call_leave))
        .route("/{room_id}/call/end", post(routes::room::call_end))
        .route("/{room_id}/call/participant", get(routes::room::participants))
        .route(
            "/{room_id}/call/message",
            get(routes::room::call_messages).post(routes::room::create_call_message),
        );

    // Message routes (under tenant/room)
    let message_routes = Router::new()
        .route("/", get(routes::message::list))
        .route("/", post(routes::message::create))
        .route("/pin", get(routes::message::pinned))
        .route("/{message_id}", put(routes::message::update))
        .route("/{message_id}", delete(routes::message::delete))
        .route("/{message_id}/pin", put(routes::message::toggle_pin))
        .route("/{message_id}/thread", get(routes::message::thread_replies))
        .route("/{message_id}/reaction", post(routes::reaction::add))
        .route(
            "/{message_id}/reaction/{emoji}",
            delete(routes::reaction::remove),
        );

    // Recording routes (under room)
    let recording_routes = Router::new()
        .route("/", get(routes::recording::list))
        .route("/", post(routes::recording::create))
        .route("/{recording_id}", delete(routes::recording::delete));

    // Transcription routes (under room)
    let transcription_routes = Router::new()
        .route("/", get(routes::transcription::list))
        .route("/", post(routes::transcription::create))
        .route("/{transcription_id}", get(routes::transcription::get));

    // Room file routes (100 MB body limit for audio uploads)
    let room_file_routes = Router::new()
        .route("/", get(routes::file::list))
        .route("/upload", post(routes::file::upload_room))
        .layer(DefaultBodyLimit::max(100 * 1024 * 1024));

    // File-by-ID routes (under tenant — no room prefix needed)
    let file_by_id_routes = Router::new()
        .route("/upload", post(routes::file::upload))
        .route("/{file_id}", get(routes::file::get))
        .route("/{file_id}/download", get(routes::file::download))
        .route("/{file_id}", delete(routes::file::delete))
        .route(
            "/{file_id}/recognize",
            post(routes::integration::recognize_file),
        )
        .layer(DefaultBodyLimit::max(100 * 1024 * 1024));

    // Background task routes (under tenant)
    let task_routes = Router::new()
        .route("/", get(routes::background_task::list))
        .route("/{task_id}", get(routes::background_task::get))
        .route("/{task_id}/download", get(routes::background_task::download));

    // Export routes (under tenant)
    let export_routes = Router::new()
        .route("/conversation", post(routes::export::export_conversation))
        .route(
            "/conversation-pdf",
            post(routes::integration::export_conversation_pdf),
        );

    // Public invite routes (no auth required for info, auth required for accept)
    let public_invite_routes = Router::new()
        .route("/{code}", get(routes::invite::get_invite_info))
        .route("/{code}/accept", post(routes::invite::accept_invite));

    // Tenant-scoped invite routes
    let tenant_invite_routes = Router::new()
        .route("/", get(routes::invite::list_invites))
        .route("/", post(routes::invite::create_invite))
        .route("/{invite_id}", delete(routes::invite::revoke_invite));

    // OAuth routes (no auth required)
    let oauth_routes = Router::new()
        .route("/{provider}", get(routes::oauth::oauth_redirect))
        .route("/callback/{provider}", get(routes::oauth::oauth_callback));

    // Stripe routes
    let stripe_routes = Router::new()
        .route("/plans", get(routes::stripe::get_plans))
        .route("/checkout", post(routes::stripe::create_checkout))
        .route("/portal", post(routes::stripe::create_portal))
        .route("/webhook", post(routes::stripe::webhook));

    // Giphy proxy routes
    let giphy_routes = Router::new()
        .route("/search", get(routes::giphy::search))
        .route("/trending", get(routes::giphy::trending));

    // Compose API
    let api = Router::new()
        .nest("/auth", auth_routes)
        .nest("/oauth", oauth_routes)
        .nest("/stripe", stripe_routes)
        .nest("/invite", public_invite_routes)
        .nest("/giphy", giphy_routes)
        .nest("/tenant", tenant_routes)
        .nest("/tenant/{tenant_id}/member", member_routes)
        .nest("/tenant/{tenant_id}/invite", tenant_invite_routes)
        .nest("/tenant/{tenant_id}/room", room_routes)
        .nest(
            "/tenant/{tenant_id}/room/{room_id}/message",
            message_routes,
        )
        .nest(
            "/tenant/{tenant_id}/room/{room_id}/recording",
            recording_routes,
        )
        .nest(
            "/tenant/{tenant_id}/room/{room_id}/transcript",
            transcription_routes,
        )
        .nest(
            "/tenant/{tenant_id}/room/{room_id}/file",
            room_file_routes,
        )
        .nest("/tenant/{tenant_id}/file", file_by_id_routes)
        .nest("/tenant/{tenant_id}/task", task_routes)
        .nest("/tenant/{tenant_id}/export", export_routes);

    // Health check
    let health = Router::new().route("/health", get(health_check));

    Router::new()
        .nest("/api", api)
        .merge(health)
        .route("/ws", get(ws::handler::ws_upgrade))
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state)
}

async fn health_check() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

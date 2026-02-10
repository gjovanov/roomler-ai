pub mod error;
pub mod extractors;
pub mod middleware;
pub mod routes;
pub mod state;
pub mod ws;

use axum::{
    Router,
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

    // Channel routes (under tenant)
    let channel_routes = Router::new()
        .route("/", get(routes::channel::list))
        .route("/", post(routes::channel::create))
        .route("/explore", get(routes::channel::explore))
        .route("/{channel_id}", get(routes::channel::get))
        .route("/{channel_id}", put(routes::channel::update))
        .route("/{channel_id}", delete(routes::channel::delete))
        .route("/{channel_id}/join", post(routes::channel::join))
        .route("/{channel_id}/leave", post(routes::channel::leave))
        .route("/{channel_id}/member", get(routes::channel::members));

    // Message routes (under tenant/channel)
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

    // Conference routes (under tenant)
    let conference_routes = Router::new()
        .route("/", get(routes::conference::list))
        .route("/", post(routes::conference::create))
        .route("/{conference_id}", get(routes::conference::get))
        .route("/{conference_id}/start", post(routes::conference::start))
        .route("/{conference_id}/join", post(routes::conference::join))
        .route("/{conference_id}/leave", post(routes::conference::leave))
        .route("/{conference_id}/end", post(routes::conference::end))
        .route(
            "/{conference_id}/participant",
            get(routes::conference::participants),
        );

    // Recording routes (under conference)
    let recording_routes = Router::new()
        .route("/", get(routes::recording::list))
        .route("/", post(routes::recording::create))
        .route("/{recording_id}", delete(routes::recording::delete));

    // Transcription routes (under conference)
    let transcription_routes = Router::new()
        .route("/", get(routes::transcription::list))
        .route("/", post(routes::transcription::create))
        .route("/{transcription_id}", get(routes::transcription::get));

    // File routes (under tenant)
    let file_routes = Router::new()
        .route("/file/upload", post(routes::file::upload))
        .route("/file/{file_id}", get(routes::file::get))
        .route("/file/{file_id}/download", get(routes::file::download))
        .route("/file/{file_id}", delete(routes::file::delete))
        .route(
            "/file/{file_id}/recognize",
            post(routes::integration::recognize_file),
        )
        .route("/{channel_id}/file", get(routes::file::list));

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

    // Tenant-scoped invite routes (require INVITE_MEMBERS permission)
    let tenant_invite_routes = Router::new()
        .route("/", get(routes::invite::list_invites))
        .route("/", post(routes::invite::create_invite))
        .route("/{invite_id}", delete(routes::invite::revoke_invite));

    // OAuth routes (no auth required)
    let oauth_routes = Router::new()
        .route("/{provider}", get(routes::oauth::oauth_redirect))
        .route("/callback/{provider}", get(routes::oauth::oauth_callback));

    // Compose API
    let api = Router::new()
        .nest("/auth", auth_routes)
        .nest("/oauth", oauth_routes)
        .nest("/invite", public_invite_routes)
        .nest("/tenant", tenant_routes)
        .nest("/tenant/{tenant_id}/member", member_routes)
        .nest("/tenant/{tenant_id}/invite", tenant_invite_routes)
        .nest("/tenant/{tenant_id}/channel", channel_routes)
        .nest(
            "/tenant/{tenant_id}/channel/{channel_id}/message",
            message_routes,
        )
        .nest("/tenant/{tenant_id}/conference", conference_routes)
        .nest(
            "/tenant/{tenant_id}/conference/{conference_id}/recording",
            recording_routes,
        )
        .nest(
            "/tenant/{tenant_id}/conference/{conference_id}/transcript",
            transcription_routes,
        )
        .nest("/tenant/{tenant_id}/channel", file_routes)
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

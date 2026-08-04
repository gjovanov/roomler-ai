pub mod cluster;
pub mod error;
pub mod extractors;
pub mod middleware;
pub mod relay_load;
pub mod routes;
pub mod state;
pub mod storage;
pub mod ws;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{delete, get, post, put},
};
use middleware::{
    auth_rate_limit::{AuthRateLimitState, AuthRateLimiter},
    client_ip::TrustedProxyIpKeyExtractor,
};
use state::AppState;
use std::sync::Arc;
use tower_governor::{GovernorLayer, governor::GovernorConfigBuilder};
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};

fn build_cors_layer(origins: &[String], frontend_url: &str) -> CorsLayer {
    // Explicit "*" = the operator deliberately chose permissive mode.
    if origins.iter().any(|o| o == "*") {
        tracing::warn!("CORS is configured fully permissive (cors_origins contains \"*\")");
        return CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);
    }

    let mut allowed: Vec<axum::http::HeaderValue> =
        origins.iter().filter_map(|o| o.parse().ok()).collect();
    // Tightened default (2026-07-28, closes the "Any-origin fallback"
    // Known Issue): with no cors_origins configured, allow only the
    // frontend's own origin instead of every origin. Same-origin app
    // traffic never needed CORS; native clients (agent, tunnel CLI,
    // desktop) don't enforce it — so nothing legitimate relied on Any.
    if origins.is_empty()
        && let Ok(v) = frontend_url.trim_end_matches('/').parse()
    {
        allowed.push(v);
    }
    if allowed.is_empty() {
        // Nothing parseable — fail open rather than brick every browser
        // client, but say so loudly.
        tracing::warn!("CORS origin list resolved empty; falling back to permissive");
        return CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);
    }
    // NOTE: with allow_credentials(true), tower-http (0.6+) rejects
    // wildcard methods/headers at request time — the old
    // `.allow_methods(Any)` here is why the two cors_tests sat in the
    // known-failing baseline. Enumerate instead.
    use axum::http::{Method, header};
    CorsLayer::new()
        .allow_origin(allowed)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE, header::ACCEPT])
        .allow_credentials(true)
}

pub fn build_router(state: AppState) -> Router {
    let cors = build_cors_layer(
        &state.settings.app.cors_origins,
        &state.settings.app.frontend_url,
    );

    // Capacity limiting for `/api`, keyed on the client address.
    //
    // `per_millisecond` (like `per_second`) takes the interval that
    // replenishes ONE token, not a rate — so the req/s setting is converted
    // by `rate_limit_period_ms` rather than passed straight through. Feeding
    // a rate into `per_second` is what made prod 10x stricter instead of 10x
    // looser on 2026-07-28.
    //
    // The key comes from `TrustedProxyIpKeyExtractor`, not `SmartIpKeyExtractor`:
    // the latter believes a client-supplied `X-Forwarded-For`, which is a
    // one-header bypass of the whole limiter.
    let governor_conf = GovernorConfigBuilder::default()
        .per_millisecond(state.settings.app.rate_limit_period_ms())
        .burst_size(state.settings.app.rate_limit_burst)
        .key_extractor(TrustedProxyIpKeyExtractor {
            trusted_hops: state.settings.app.rate_limit_trusted_hops,
        })
        .finish()
        .expect("rate limit period is clamped non-zero, so the config is valid");
    let governor_layer = GovernorLayer {
        config: governor_conf.into(),
    };

    // Brute-force gate for the credential endpoints, keyed on
    // (client address, account) so it can be strict without locking out
    // everyone sharing an address.
    let auth_rate_limit_state = AuthRateLimitState {
        limiter: Arc::new(AuthRateLimiter::new(
            state.settings.app.auth_rate_limit_period_ms(),
            state.settings.app.auth_rate_limit_burst,
        )),
        trusted_hops: state.settings.app.rate_limit_trusted_hops,
    };

    // Password endpoints — the ones actually worth guessing at — carry the
    // per-(address, account) brute-force gate on top of the general limiter.
    //
    // `/refresh` is deliberately NOT here: a refresh token is a signed,
    // high-entropy secret rather than something guessable, so the gate would
    // buy nothing, and refreshes carry no account field to key on. Every
    // refresh from one address would share a single bucket, which turns a
    // token-expiry stampede in a shared office into a self-inflicted outage.
    let credential_routes = Router::new()
        .route("/register", post(routes::auth::register))
        .route("/login", post(routes::auth::login))
        .layer(axum::middleware::from_fn_with_state(
            auth_rate_limit_state,
            middleware::auth_rate_limit::auth_rate_limit,
        ));

    // Auth routes (no tenant prefix)
    let auth_routes = Router::new()
        .route("/logout", post(routes::auth::logout))
        .route("/refresh", post(routes::auth::refresh))
        .route("/activate", post(routes::auth::activate))
        .route("/me", get(routes::auth::me))
        .route("/me", put(routes::auth::me))
        .merge(credential_routes);

    // Tenant routes
    let tenant_routes = Router::new()
        .route("/", get(routes::tenant::list))
        .route("/", post(routes::tenant::create))
        .route("/{tenant_id}", get(routes::tenant::get));

    // Member routes (under tenant)
    let member_routes = Router::new()
        .route(
            "/",
            get(routes::user::list_members).post(routes::invite::add_member),
        )
        .route("/me", get(routes::user::my_membership));

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
        .route(
            "/{room_id}/call/participant",
            get(routes::room::participants),
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
        )
        .route("/read", post(routes::message::mark_read))
        .route("/read-all", post(routes::message::read_all))
        .route("/unread-count", get(routes::message::unread_count));

    // Recording routes (under room)
    let recording_routes = Router::new()
        .route("/", get(routes::recording::list))
        .route("/", post(routes::recording::create))
        .route("/{recording_id}", delete(routes::recording::delete));

    // Room file routes (100 MB body limit for audio uploads)
    let room_file_routes = Router::new()
        .route("/", get(routes::file::list))
        .route("/upload", post(routes::file::upload_room))
        .layer(DefaultBodyLimit::max(100 * 1024 * 1024));

    // File-by-ID routes (under tenant — no room prefix needed)
    let file_by_id_routes = Router::new()
        .route("/", get(routes::file::list_tenant_files))
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
        .route(
            "/{task_id}/download",
            get(routes::background_task::download),
        );

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

    // Role routes (under tenant)
    let role_routes = Router::new()
        .route("/", get(routes::role::list))
        .route("/", post(routes::role::create))
        .route("/{role_id}", put(routes::role::update))
        .route("/{role_id}", delete(routes::role::delete))
        .route("/{role_id}/assign/{user_id}", post(routes::role::assign))
        .route(
            "/{role_id}/assign/{user_id}",
            delete(routes::role::unassign),
        );

    // Tenant-scoped invite routes
    let tenant_invite_routes = Router::new()
        .route("/", get(routes::invite::list_invites))
        .route("/", post(routes::invite::create_invite))
        .route("/batch", post(routes::invite::batch_create_invite))
        .route("/{invite_id}", delete(routes::invite::revoke_invite));

    // OAuth routes (no auth required)
    let oauth_routes = Router::new()
        .route("/{provider}", get(routes::oauth::oauth_redirect))
        .route("/callback/{provider}", get(routes::oauth::oauth_callback));

    // Stripe routes
    // S5 — the webhook route is NOT here: Stripe's retry bursts must
    // never hit the per-IP governor (a 429'd delivery marks the endpoint
    // failing on their side). It's mounted un-governed in the outer
    // router below; its own HMAC signature check is the auth.
    let stripe_routes = Router::new()
        .route("/plans", get(routes::stripe::get_plans))
        .route("/checkout", post(routes::stripe::create_checkout))
        .route("/portal", post(routes::stripe::create_portal));

    // Giphy proxy routes
    let giphy_routes = Router::new()
        .route("/search", get(routes::giphy::search))
        .route("/trending", get(routes::giphy::trending));

    // Push notification routes (user-scoped, no tenant prefix)
    let push_routes = Router::new()
        .route("/config", get(routes::push::config))
        .route("/subscribe", post(routes::push::subscribe))
        .route("/unsubscribe", post(routes::push::unsubscribe));

    // Notification routes (user-scoped, no tenant prefix)
    let notification_routes = Router::new()
        .route("/", get(routes::notification::list))
        .route("/unread", get(routes::notification::unread))
        .route("/unread-count", get(routes::notification::unread_count))
        .route(
            "/{notification_id}/read",
            put(routes::notification::mark_read),
        )
        .route("/read-all", post(routes::notification::mark_all_read));

    // User profile routes
    let user_routes = Router::new()
        .route("/me", put(routes::user::update_profile))
        // P4 — static segment; wins over the `{user_id}` capture below.
        .route("/unread-summary", get(routes::user::unread_summary))
        .route("/{user_id}", get(routes::user::get_profile));

    // rc.58 — browser console log batch ingest. User-authed (the
    // controller user's JWT). The body MUST include an explicit
    // `tenant_id` field since the user JWT doesn't pin a tenant; the
    // route handler verifies membership before persisting.
    let log_routes = Router::new().route("/browser", post(routes::agent_log::ingest_browser));

    // Search routes (under tenant)
    let search_routes = Router::new().route("/", get(routes::search::search));

    // Remote-control agent routes (tenant-scoped)
    let agent_routes = Router::new()
        .route("/", get(routes::remote_control::list_agents))
        .route(
            "/enroll-token",
            post(routes::remote_control::issue_enrollment_token),
        )
        // S1a — operator-forced self-update: bulk (static segment wins over
        // the `{agent_id}` param below) + per-agent.
        .route(
            "/update",
            post(routes::remote_control::trigger_agents_update),
        )
        .route(
            "/{agent_id}/update",
            post(routes::remote_control::trigger_agent_update),
        )
        .route(
            "/{agent_id}",
            get(routes::remote_control::get_agent)
                .put(routes::remote_control::update_agent)
                .delete(routes::remote_control::delete_agent),
        )
        .route(
            "/{agent_id}/crash",
            get(routes::agent_crash::list_for_agent),
        )
        // rc.58 — centralized log batch ingest + listing. Agent
        // uploader POSTs here with its agent JWT; admin UI GETs to
        // view recent batches. Route is under the `/agent/{agent_id}`
        // tenant-scoped tree so the standard tenant-membership check
        // applies for reads, and the agent JWT's `tenant_id` +
        // `sub` claims are matched against the route params on writes.
        .route(
            "/{agent_id}/logs",
            post(routes::agent_log::ingest_agent).get(routes::agent_log::list_for_agent),
        );

    // Remote-control session routes (tenant-scoped)
    let remote_session_routes = Router::new()
        .route("/{session_id}", get(routes::remote_control::get_session))
        .route(
            "/{session_id}/terminate",
            post(routes::remote_control::terminate_session),
        )
        .route(
            "/{session_id}/audit",
            get(routes::remote_control::session_audit),
        );

    // Public agent endpoints: enrollment uses an admin-issued enrollment
    // token (no user JWT); /latest-release is unauthenticated because
    // the agent's auto-updater calls it before any session and the
    // payload is already public via github.com/.../releases — we just
    // proxy + cache so a fleet of agents behind one IP doesn't blow
    // past GitHub's 60-req/hr unauth quota.
    let public_agent_routes = Router::new()
        .route("/enroll", post(routes::remote_control::enroll_agent))
        .route(
            "/latest-release",
            get(routes::agent_release::latest_release),
        )
        .route(
            "/installer/{flavour}/health",
            get(routes::agent_release::installer_health),
        )
        .route(
            "/installer/{flavour}",
            get(routes::agent_release::installer_proxy),
        )
        // Agent crash-report ingest (Task 9 Phase 2). Public because
        // the agent authenticates by its own JWT in the Authorization
        // header — same security posture as the WS `/ws?role=agent`
        // upgrade, and the user-JWT extractor isn't applicable here.
        // The route handler calls `state.auth.verify_agent_token`
        // directly.
        .route("/crash", post(routes::agent_crash::ingest));

    // Public owner-consent routes (Phase 4). No auth extractor — the unguessable
    // token IS the capability; the handler validates it, single-uses it, and
    // resolves the session's consent slot via the Hub.
    let public_consent_routes = Router::new()
        .route("/{token}/approve", post(routes::consent::approve_consent))
        .route("/{token}/deny", post(routes::consent::deny_consent));

    // roomler-tunnel routes — same enrollment two-step shape as the
    // agent, but a distinct audience (`TunnelClient` JWT) so a leaked
    // agent token can't impersonate a client and vice-versa. CRUD +
    // policy + audit endpoints land in T2.
    let tunnel_client_routes = Router::new()
        .route("/", get(routes::tunnel::list_tunnel_clients))
        .route(
            "/enroll-token",
            post(routes::tunnel::issue_tunnel_enrollment_token),
        )
        // matchit gives the static `/enroll-token` above precedence over this
        // parameterised segment, so there is no shadowing.
        .route("/{client_id}", delete(routes::tunnel::delete_tunnel_client));
    let tunnel_policy_routes = Router::new()
        .route(
            "/",
            get(routes::tunnel::list_tunnel_policies).post(routes::tunnel::create_tunnel_policy),
        )
        .route(
            "/{policy_id}",
            get(routes::tunnel::get_tunnel_policy)
                .put(routes::tunnel::update_tunnel_policy)
                .delete(routes::tunnel::delete_tunnel_policy),
        );
    // Phase 1 subnet router — admin approves which advertised routes a node may
    // actually route for peers.
    let overlay_node_routes = Router::new()
        .route("/", get(routes::overlay_route::list_overlay_nodes))
        .route(
            "/{node_id}/approved-routes",
            put(routes::overlay_route::set_approved_routes),
        )
        .route(
            "/{node_id}/exit-node",
            put(routes::overlay_route::set_exit_node),
        )
        // Evict a node from the mesh + release its address back to the pool.
        .route(
            "/{node_id}",
            delete(routes::overlay_route::evict_overlay_node),
        );

    // Overlay L3 ACL — shapes the netmap each node receives. `/mode` carries
    // the tenant-wide posture (off | warn | enforce); it is `off` by default so
    // deploying the feature can never black-hole a live mesh.
    let overlay_policy_routes = Router::new()
        .route(
            "/",
            get(routes::overlay_policy::list).post(routes::overlay_policy::create),
        )
        .route(
            "/mode",
            get(routes::overlay_policy::get_mode).put(routes::overlay_policy::set_mode),
        )
        .route(
            "/{policy_id}",
            get(routes::overlay_policy::get)
                .put(routes::overlay_policy::update)
                .delete(routes::overlay_policy::delete),
        );

    // Phase 2 MagicDNS — the tenant's overlay DNS domain + upstreams.
    let magic_dns_routes = Router::new().route(
        "/",
        get(routes::overlay_route::get_magic_dns).put(routes::overlay_route::set_magic_dns),
    );

    let public_tunnel_routes = Router::new()
        .route("/enroll", post(routes::tunnel::enroll_tunnel_client))
        // Auth is in-handler (TunnelClient bearer token), so it rides the
        // "public" (no user-JWT-middleware) tunnel router.
        .route("/agents", get(routes::tunnel::list_tenant_agents));

    // `/api/tunnel/{latest-release,installer/{platform}}` — public
    // GitHub-Releases proxy for the roomler-tunnel binary. Same
    // shape + lifecycle as `/api/agent/{latest-release,installer/...}`
    // but a separate namespace so the two artifact sets don't collide.
    let public_tunnel_release_routes = Router::new()
        .route(
            "/latest-release",
            get(routes::tunnel_release::latest_release),
        )
        .route(
            "/installer/{platform}/health",
            get(routes::tunnel_release::installer_health),
        )
        .route(
            "/installer/{platform}",
            get(routes::tunnel_release::installer_proxy),
        );

    // `/api/setup/{latest-release,{platform}/health,{platform}}` —
    // public proxy for the unified `roomler-setup` wizard (tag
    // `setup-v*`, live since setup-v0.3.0-rc.196; the legacy
    // `/api/tunnel-wizard/*` family was retired in P4c-2).
    // NB `/install.sh` + `/install.ps1` are static segments — axum's
    // matchit gives them precedence over the `/{platform}` param, so
    // the script endpoints never shadow (or get shadowed by) the
    // artifact proxy.
    let public_setup_routes = Router::new()
        .route(
            "/latest-release",
            get(routes::setup_release::setup_latest_release),
        )
        .route("/install.sh", get(routes::setup_release::install_script_sh))
        .route(
            "/install.ps1",
            get(routes::setup_release::install_script_ps1),
        )
        .route(
            "/{platform}/health",
            get(routes::setup_release::setup_installer_health),
        )
        .route(
            "/{platform}",
            get(routes::setup_release::setup_installer_proxy),
        );

    // TURN credentials (user-scoped, no tenant prefix)
    let turn_routes = Router::new().route(
        "/credentials",
        get(routes::remote_control::turn_credentials),
    );
    // Multi-region relay PoP topology (user-scoped, read-only, no secrets).
    let relay_routes = Router::new().route("/regions", get(routes::remote_control::relay_regions));

    // Compose API
    let api = Router::new()
        // C-6 — per-pod cluster status (identity, counters, gauges).
        .route("/cluster/status", get(routes::cluster::status))
        .nest("/auth", auth_routes)
        .nest("/user", user_routes)
        .nest("/oauth", oauth_routes)
        .nest("/stripe", stripe_routes)
        .nest("/invite", public_invite_routes)
        .nest("/giphy", giphy_routes)
        .nest("/push", push_routes)
        .nest("/notification", notification_routes)
        .nest("/agent", public_agent_routes)
        .nest("/consent", public_consent_routes)
        .nest("/tunnel-client", public_tunnel_routes)
        .nest("/tunnel", public_tunnel_release_routes)
        .nest("/setup", public_setup_routes)
        .nest("/turn", turn_routes)
        .nest("/relay", relay_routes)
        .nest("/log", log_routes)
        .nest("/tenant", tenant_routes)
        .nest("/tenant/{tenant_id}/member", member_routes)
        .nest("/tenant/{tenant_id}/role", role_routes)
        .nest("/tenant/{tenant_id}/invite", tenant_invite_routes)
        .nest("/tenant/{tenant_id}/search", search_routes)
        .nest("/tenant/{tenant_id}/room", room_routes)
        .nest("/tenant/{tenant_id}/room/{room_id}/message", message_routes)
        .nest(
            "/tenant/{tenant_id}/room/{room_id}/recording",
            recording_routes,
        )
        .nest("/tenant/{tenant_id}/room/{room_id}/file", room_file_routes)
        .nest("/tenant/{tenant_id}/file", file_by_id_routes)
        .nest("/tenant/{tenant_id}/task", task_routes)
        .nest("/tenant/{tenant_id}/export", export_routes)
        .nest("/tenant/{tenant_id}/agent", agent_routes)
        .nest("/tenant/{tenant_id}/tunnel-client", tunnel_client_routes)
        .nest("/tenant/{tenant_id}/tunnel-policy", tunnel_policy_routes)
        .nest("/tenant/{tenant_id}/overlay-node", overlay_node_routes)
        .nest("/tenant/{tenant_id}/overlay-acl", overlay_policy_routes)
        .nest("/tenant/{tenant_id}/magic-dns", magic_dns_routes)
        .nest("/tenant/{tenant_id}/session", remote_session_routes);

    // Health check. `/health` stays a cheap process-alive 200 (liveness /
    // startup probes — must NOT flap on dependency blips or k8s restarts the
    // pod during a Redis outage); `/health/ready` checks the dependencies
    // (readiness probe + operators).
    let health = Router::new()
        .route("/health", get(health_check))
        .route("/health/ready", get(readiness_check));

    // Apply rate limiting only to API routes (not health/ws which need unrestricted access)
    let rate_limited_api = Router::new().nest("/api", api).layer(governor_layer);

    Router::new()
        .merge(rate_limited_api)
        .merge(health)
        // S5 — Stripe webhook outside the governor (signature-authed;
        // retry bursts from Stripe's fixed IPs would trip the per-IP
        // limiter and mark the endpoint failing on their dashboard).
        .route("/api/stripe/webhook", post(routes::stripe::webhook))
        .route("/ws", get(ws::handler::ws_upgrade))
        .route("/derp", get(ws::derp::derp_upgrade))
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

/// Readiness: 200 only when Mongo answers a ping, the Redis publisher
/// round-trips a PING, and the pub/sub subscriber currently holds a live
/// subscription. 503 otherwise, with per-check detail in the body. A pod
/// that booted without Redis (`redis_pubsub: None`) reports not-ready —
/// cross-instance delivery is silently broken in that state and the old
/// static 200 hid it.
async fn readiness_check(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    use std::time::Duration;

    let mongo_ok = tokio::time::timeout(
        Duration::from_secs(2),
        state.db.run_command(bson::doc! { "ping": 1 }),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false);

    let (redis_ok, subscriber_ok) = match &state.redis_pubsub {
        Some(pubsub) => {
            let ok = tokio::time::timeout(Duration::from_secs(2), pubsub.ping())
                .await
                .map(|r| r.is_ok())
                .unwrap_or(false);
            (
                ok,
                state
                    .redis_sub_alive
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
        }
        None => (false, false),
    };

    let ready = mongo_ok && redis_ok && subscriber_ok;
    let status = if ready {
        axum::http::StatusCode::OK
    } else {
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        axum::Json(serde_json::json!({
            "status": if ready { "ready" } else { "not_ready" },
            "checks": {
                "mongo": mongo_ok,
                "redis": redis_ok,
                "redis_subscriber": subscriber_ok,
            },
            "version": env!("CARGO_PKG_VERSION"),
        })),
    )
}

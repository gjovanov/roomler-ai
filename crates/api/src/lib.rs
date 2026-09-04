// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
pub mod cluster;
pub mod compose;
pub mod core_state;
pub mod extractors;
pub mod media_stats;
pub mod middleware;
pub mod routes;
pub mod state;
pub mod stats_rollup;
pub mod ws;

// FR-69 P1b/P1d — these moved into `roomler-core` unchanged (they hold or
// serve `Core`'s fields, or are the request-side primitives a module's
// handlers need: the error type, the session cookie helpers, the origin
// policy); re-exported so every `crate::…` path in this crate reads as before.
pub use roomler_core::{cookies, error, origin, rate_limit, relay_load, storage, user_analytics};

use axum::{
    Router,
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
    // The allow-list is computed by `origin::origin_policy`, which the `/ws`
    // upgrade also consults before honouring a session COOKIE. Sharing the
    // answer is the point: two hand-rolled copies of "an origin we trust"
    // would drift, and the copy that drifts loose is an authenticated socket
    // for somebody else's page.
    //
    // Tightened default (2026-07-28, closes the "Any-origin fallback"
    // Known Issue): with no cors_origins configured, allow only the
    // frontend's own origin instead of every origin. Same-origin app
    // traffic never needed CORS; native clients (agent, tunnel CLI,
    // desktop) don't enforce it — so nothing legitimate relied on Any.
    let policy = crate::origin::origin_policy(origins, frontend_url);

    // Explicit "*" = the operator deliberately chose permissive mode.
    let allowed_strs = match policy {
        crate::origin::OriginPolicy::AnyOrigin => {
            tracing::warn!("CORS is configured fully permissive (cors_origins contains \"*\")");
            return CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any);
        }
        crate::origin::OriginPolicy::Only(list) => list,
    };

    let allowed: Vec<axum::http::HeaderValue> =
        allowed_strs.iter().filter_map(|o| o.parse().ok()).collect();
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
        .route("/{tenant_id}", get(routes::tenant::get))
        // Owner-only. Archiving revokes every device's enrollment and
        // releases the org's mesh — see `routes::tenant::archive`.
        .route("/{tenant_id}/archive", post(routes::tenant::archive))
        .route("/{tenant_id}/unarchive", post(routes::tenant::unarchive));

    // Member routes (under tenant)
    let member_routes = Router::new()
        .route(
            "/",
            get(routes::user::list_members).post(routes::invite::add_member),
        )
        .route("/me", get(routes::user::my_membership))
        // FR-10 — matchit gives the static `/me` above precedence over this
        // parameterised segment.
        .route("/{user_id}", delete(routes::user::remove_member));

    // Room CRUD, messages, reactions, files, search, the xlsx export and
    // Giphy are the `chat` module's (FR-69 P3). The call endpoints below
    // share the `/room` prefix with it and are conference's — they stay here
    // until P4.
    let call_routes = Router::new()
        .route("/{room_id}/call/start", post(routes::call::call_start))
        .route("/{room_id}/call/join", post(routes::call::call_join))
        .route("/{room_id}/call/leave", post(routes::call::call_leave))
        .route("/{room_id}/call/end", post(routes::call::call_end))
        .route(
            "/{room_id}/call/participant",
            get(routes::call::participants),
        );

    // Recording routes (under room)
    let recording_routes = Router::new()
        .route("/", get(routes::recording::list))
        .route("/", post(routes::recording::create))
        .route("/{recording_id}", delete(routes::recording::delete));

    // Background task routes (under tenant)
    let task_routes = Router::new()
        .route("/", get(routes::background_task::list))
        .route("/{task_id}", get(routes::background_task::get))
        .route(
            "/{task_id}/download",
            get(routes::background_task::download),
        );

    // Export routes (under tenant). The xlsx `/conversation` export is the
    // `chat` module's; this PDF one is the host's integration route, mounted
    // under the same prefix.
    let export_routes = Router::new().route(
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

    // Stripe routes are the `saas` module's (FR-69 P2), including the
    // webhook, which that module mounts un-governed at the root.

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
        // P4 — `/user/unread-summary` is the `chat` module's (it counts
        // messages); a static segment, it still wins over the capture.
        // FR-12 P3 — likewise static, likewise above the capture.
        .route("/tutorial", put(routes::user::update_tutorial))
        // FR-58 P4 — `/user/newsletter` (the signed-in toggle) is mounted by
        // the `saas` module; a static segment, it still wins over the capture.
        .route("/{user_id}", get(routes::user::get_profile));

    // rc.58 — browser console log batch ingest. User-authed (the
    // controller user's JWT). The body MUST include an explicit
    // `tenant_id` field since the user JWT doesn't pin a tenant; the
    // route handler verifies membership before persisting.
    let log_routes = Router::new().route("/browser", post(routes::agent_log::ingest_browser));

    // Remote-control agent routes (tenant-scoped)
    let agent_routes = Router::new()
        .route("/", get(routes::remote_control::list_agents))
        .route(
            "/enroll-token",
            post(routes::remote_control::issue_enrollment_token),
        )
        // FR-51 P2 — reusable ephemeral enrollment keys (static segments win
        // over the `{agent_id}` param below, same as `/enroll-token`).
        .route(
            "/enroll-key",
            get(routes::enroll_key::list_keys).post(routes::enroll_key::mint_key),
        )
        .route(
            "/enroll-key/{key_id}",
            delete(routes::enroll_key::revoke_key),
        )
        .route(
            "/enroll-key/{key_id}/uses",
            get(routes::enroll_key::list_key_uses),
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
        // FR-40 — order the device to retire its overlay key (MANAGE_AGENTS;
        // cap-gated, audited, queued for an offline device).
        .route(
            "/{agent_id}/overlay-key/rotate",
            post(routes::overlay_key::rotate_overlay_key),
        )
        // Multi-org — add an already-enrolled device to a SECOND org from the
        // UI (MANAGE_AGENTS in both). `join-targets` is the picker's source
        // of truth so the dialog can never offer a choice that 403s.
        .route("/{agent_id}/join-org", post(routes::agent_org::join_org))
        .route(
            "/{agent_id}/join-targets",
            get(routes::agent_org::join_targets),
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
        )
        // Fleet RPC — remote command execution. `/exec` (the fleet sweep) and
        // `/{agent_id}/exec` (one device) sit at different depths, so they
        // can't shadow each other whatever the declaration order.
        .route("/exec", post(routes::agent_exec::exec_bulk))
        .route("/{agent_id}/exec", post(routes::agent_exec::exec))
        .route(
            "/{agent_id}/exec/{request_id}/cancel",
            post(routes::agent_exec::cancel),
        )
        // Gate 3 — the per-device policy. MANAGE_AGENTS, not EXEC_DEVICE:
        // deciding a device MAY be exec'd is a management act; performing
        // the exec is a separate power.
        .route(
            "/{agent_id}/exec-policy",
            put(routes::agent_exec::set_policy),
        )
        // Roomler SSH — ask for a session on one device. Answers 200 with
        // either where to dial or which gate refused; a denial is a policy
        // outcome the caller must read, not a transport failure.
        .route("/{agent_id}/ssh", post(routes::agent_ssh::request_session))
        // Gate 3's twin, and the same MANAGE_AGENTS-not-SSH_DEVICE split.
        .route("/{agent_id}/ssh-policy", put(routes::agent_ssh::set_policy))
        // Remote config — records an INTENT for the device to reconcile; the
        // device may refuse it (docs/remote-config.md).
        .route(
            "/{agent_id}/desired-config",
            put(routes::remote_config::set_desired_config),
        )
        // FR-19 gate 3 — approve a device as an org relay. MANAGE_AGENTS +
        // EXEC_DEVICE (there is no free permission bit; routes/peer_relay.rs
        // says why), audited on both arms.
        .route(
            "/{agent_id}/peer-relay-policy",
            put(routes::peer_relay::set_policy),
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
        // Agent crash-report ingest (Task 9 Phase 2). "Public" only in the
        // sense that the user-JWT extractor does not apply: the agent
        // authenticates with its own JWT in the Authorization header, via the
        // `AuthAgent` extractor — which also confirms the agent's row is still
        // one we accept (deleted / quarantined ⇒ 401), the same rule the WS
        // `/ws?role=agent` upgrade applies. Until #667 this handler read the
        // claims and nothing else, so a deleted device kept writing for the
        // remaining year of its token.
        .route("/crash", post(routes::agent_crash::ingest))
        // FR-51 P3 — an ephemeral device removes itself on clean shutdown.
        // Agent-JWT authenticated (`AuthAgent`); a permanent row is refused.
        .route(
            "/self/unenroll",
            post(routes::remote_control::self_unenroll),
        );

    // Public owner-consent routes (Phase 4). No auth extractor — the unguessable
    // token IS the capability; the handler validates it, single-uses it, and
    // resolves the session's consent slot via the Hub.
    let public_consent_routes = Router::new()
        .route("/{token}/approve", post(routes::consent::approve_consent))
        .route("/{token}/deny", post(routes::consent::deny_consent));

    // roomler-cli routes — same enrollment two-step shape as the
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
        .route(
            "/{client_id}",
            put(routes::tunnel::update_tunnel_client).delete(routes::tunnel::delete_tunnel_client),
        );
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

    // Fleet RPC — the org kill-switch (gate 1) and the attempt log. Both are
    // tenant-scoped rather than per-device: the switch governs every device,
    // and the log is the org-wide "what ran on my fleet?" view.
    let exec_audit_routes = Router::new().route("/", get(routes::agent_exec::audit));
    // FR-19 — peer relays: the org switch + approved-relay listing, and the
    // decision log. Approval itself is on the agent router (gate 3 is per
    // device).
    let peer_relay_routes = Router::new().route(
        "/",
        get(routes::peer_relay::get_settings).put(routes::peer_relay::set_mode),
    );
    let peer_relay_audit_routes = Router::new().route("/", get(routes::peer_relay::audit));
    let ssh_audit_routes = Router::new().route("/", get(routes::agent_ssh::audit));
    let ssh_activity_routes = Router::new().route("/", get(routes::agent_ssh::activity));
    let exec_settings_routes = Router::new().route(
        "/",
        get(routes::agent_exec::get_org_settings).put(routes::agent_exec::set_org_settings),
    );

    // FR-51 P2 — the ephemeral-key class switch (MANAGE_TENANT, the exec/ssh
    // twins' shape): whether reusable device-minting credentials exist in
    // this org at all.
    let ephemeral_key_settings_routes = Router::new().route(
        "/",
        get(routes::enroll_key::get_org_settings).put(routes::enroll_key::set_org_settings),
    );

    // Roomler SSH — its own org kill-switch (gate 1). A separate switch from
    // the exec one on purpose: allowing bounded diagnostic commands is not
    // the same decision as allowing interactive sessions.
    let ssh_settings_routes = Router::new().route(
        "/",
        get(routes::agent_ssh::get_org_settings).put(routes::agent_ssh::set_org_settings),
    );

    // Phase 2 MagicDNS — the tenant's overlay DNS domain + upstreams.
    let magic_dns_routes = Router::new().route(
        "/",
        get(routes::overlay_route::get_magic_dns).put(routes::overlay_route::set_magic_dns),
    );

    // Multi-org P2b — the tenant's overlay address block + the renumber
    // migration onto it. `renumber` defaults to a DRY RUN.
    let overlay_block_routes = Router::new()
        .route("/", get(routes::overlay_block::get_block))
        .route("/renumber", post(routes::overlay_block::renumber))
        // FR-47 P3 — return leaked host ordinals to the recycle pool.
        // Platform-operator only (checked in-handler, like the block reclaim
        // route) and dry-run by default.
        .route(
            "/reconcile-hosts",
            post(routes::overlay_block::reconcile_hosts),
        );

    let public_tunnel_routes = Router::new()
        .route("/enroll", post(routes::tunnel::enroll_tunnel_client))
        // Auth is in-handler (TunnelClient bearer token), so it rides the
        // "public" (no user-JWT-middleware) tunnel router.
        .route("/agents", get(routes::tunnel::list_tenant_agents));

    // `/api/tunnel/{latest-release,installer/{platform}}` — public
    // GitHub-Releases proxy for the roomler CLI binary. Same
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

    // `/api/releases/refresh` — bearer-authenticated cache-bust called by
    // the release workflows on every published tag. One HTTP call busts
    // every pod (the receiving pod fans out over the C-1 bus), so a new
    // release is visible fleet-wide in seconds instead of after one TTL.
    let releases_routes =
        Router::new().route("/refresh", axum::routing::post(routes::releases::refresh));

    // TURN credentials (user-scoped, no tenant prefix)
    let turn_routes = Router::new().route(
        "/credentials",
        get(routes::remote_control::turn_credentials),
    );
    // Multi-region relay PoP topology (user-scoped, read-only, no secrets).
    let relay_routes = Router::new().route("/regions", get(routes::remote_control::relay_regions));

    // Stats PR-3 — observability queries. The /admin family is gated by
    // the platform_admins ObjectId allowlist (404 on miss); the tenant
    // family gates in-handler (member for overview, MANAGE_AGENTS for
    // the queryable series — also 404, the client logs out on 403).
    let admin_stats_routes = Router::new()
        .route("/relay/current", get(routes::stats::admin_relay_current))
        .route("/relay/history", get(routes::stats::admin_relay_history))
        .route("/orgs", get(routes::stats::admin_orgs))
        // FR-20 P5 - per-org metered cost + margin inputs. Platform
        // admin only, and it reports "not priced"/"not monitored"
        // rather than zero for anything it has not measured.
        .route("/cost", get(routes::cost::admin_cost))
        .route("/users", get(routes::stats::admin_users))
        .route("/machines", get(routes::stats::admin_machines))
        .route("/calls", get(routes::stats::admin_calls))
        // Wave 3 — per-user usage. `/usage/{uid}` spans every org, which is
        // what makes "in which org did this happen" answerable.
        .route("/usage", get(routes::usage::admin_usage))
        .route("/usage/{user_id}", get(routes::usage::admin_usage_detail));
    // FR-58 — the newsletter admin surface (`/admin/newsletter/*`) is
    // mounted by the `saas` module (FR-69 P2).
    let tenant_stats_routes = Router::new()
        .route("/overview", get(routes::stats::tenant_overview))
        .route("/mesh", get(routes::stats::tenant_mesh))
        .route("/machines", get(routes::stats::tenant_machines))
        .route("/calls", get(routes::stats::tenant_calls))
        .route("/tunnels", get(routes::stats::tenant_tunnels))
        // FR-20 P6 - the org's OWN metered consumption, in units rather
        // than money: these are our costs, not their bill, and the point of
        // the surface is that a high relayed share is something their own
        // network team can act on. Membership only; failures 404.
        .route("/resources", get(routes::cost::tenant_resources))
        // `/usage` needs MANAGE_AGENTS (everyone's activity); `/usage/{uid}`
        // is self-service for your OWN row, MANAGE_AGENTS for anyone else's.
        .route("/usage", get(routes::usage::tenant_usage))
        .route("/usage/{user_id}", get(routes::usage::tenant_usage_detail));

    // Compose API
    let api = Router::new()
        // FR-69 — what this server is composed of; the module list one UI
        // build and one daemon read before offering a pillar.
        .route("/capabilities", get(routes::capabilities::get))
        // C-6 — per-pod cluster status (identity, counters, gauges).
        .route("/cluster/status", get(routes::cluster::status))
        .nest("/auth", auth_routes)
        .nest("/user", user_routes)
        .nest("/oauth", oauth_routes)
        .nest("/invite", public_invite_routes)
        .nest("/push", push_routes)
        .nest("/notification", notification_routes)
        .nest("/agent", public_agent_routes)
        .nest("/consent", public_consent_routes)
        .nest("/tunnel-client", public_tunnel_routes)
        .nest("/tunnel", public_tunnel_release_routes)
        .nest("/setup", public_setup_routes)
        .nest("/releases", releases_routes)
        .nest("/turn", turn_routes)
        .nest("/relay", relay_routes)
        .nest("/admin/stats", admin_stats_routes)
        // `/stripe/*`, `/admin/newsletter/*`, `/admin/plan-compliance`,
        // `/user/newsletter` and `/subscribe*` are the `saas` module's
        // (FR-69 P2), merged in by `state.modules.mount` below.
        // The block registry is GLOBAL, so reclaiming from it is a platform
        // operation, not a tenant one. Dry-run by default.
        .route(
            "/admin/overlay-block/reclaim",
            post(routes::overlay_block::reclaim),
        )
        // FR-54 — overlay networks whose organization no longer exists.
        // Platform-operator only (checked in-handler), dry-run by default.
        .route(
            "/admin/overlay-network/orphans",
            post(routes::overlay_block::orphans),
        )
        // Wave 2 — the SPA's route-change beacon (authenticated, paths
        // normalised server-side). User-scoped, not tenant-scoped: a
        // user navigates across orgs within one session.
        .route("/stats/pageview", post(routes::stats::page_view))
        .nest("/log", log_routes)
        .nest("/tenant", tenant_routes)
        .nest("/tenant/{tenant_id}/member", member_routes)
        .nest("/tenant/{tenant_id}/role", role_routes)
        .nest("/tenant/{tenant_id}/invite", tenant_invite_routes)
        .nest("/tenant/{tenant_id}/room", call_routes)
        .nest(
            "/tenant/{tenant_id}/room/{room_id}/recording",
            recording_routes,
        )
        .nest("/tenant/{tenant_id}/task", task_routes)
        .nest("/tenant/{tenant_id}/export", export_routes)
        .nest("/tenant/{tenant_id}/agent", agent_routes)
        .nest(
            "/tenant/{tenant_id}/device",
            Router::new().route("/", get(routes::device::list_devices)),
        )
        .nest("/tenant/{tenant_id}/tunnel-client", tunnel_client_routes)
        .nest("/tenant/{tenant_id}/tunnel-policy", tunnel_policy_routes)
        .nest("/tenant/{tenant_id}/overlay-node", overlay_node_routes)
        .nest("/tenant/{tenant_id}/overlay-acl", overlay_policy_routes)
        .nest("/tenant/{tenant_id}/magic-dns", magic_dns_routes)
        .nest("/tenant/{tenant_id}/overlay-block", overlay_block_routes)
        .nest("/tenant/{tenant_id}/stats", tenant_stats_routes)
        .nest("/tenant/{tenant_id}/exec-audit", exec_audit_routes)
        .nest("/tenant/{tenant_id}/peer-relay", peer_relay_routes)
        .nest(
            "/tenant/{tenant_id}/peer-relay-audit",
            peer_relay_audit_routes,
        )
        .nest("/tenant/{tenant_id}/ssh-audit", ssh_audit_routes)
        .nest("/tenant/{tenant_id}/ssh-activity", ssh_activity_routes)
        .nest("/tenant/{tenant_id}/exec-settings", exec_settings_routes)
        .nest("/tenant/{tenant_id}/ssh-settings", ssh_settings_routes)
        .nest(
            "/tenant/{tenant_id}/ephemeral-key-settings",
            ephemeral_key_settings_routes,
        )
        .nest("/tenant/{tenant_id}/session", remote_session_routes);

    // Health check. `/health` stays a cheap process-alive 200 (liveness /
    // startup probes — must NOT flap on dependency blips or k8s restarts the
    // pod during a Redis outage); `/health/ready` checks the dependencies
    // (readiness probe + operators).
    let health = Router::new()
        .route("/health", get(health_check))
        .route("/health/ready", get(readiness_check));

    // FR-69 — the module crates' governed routes join the host's under
    // `/api` (`crates/api/src/compose.rs`).
    let api = state.modules.mount(api);

    // Apply rate limiting only to API routes (not health/ws which need unrestricted access)
    let rate_limited_api = Router::new().nest("/api", api).layer(governor_layer);

    // FR-69 — the module crates' ungoverned routes join at the root: the
    // Stripe webhook (S5) is signature-authed, and retry bursts from Stripe's
    // fixed IPs would trip the per-IP limiter and mark the endpoint failing
    // on their dashboard.
    let root = state
        .modules
        .mount_unlimited(Router::new().merge(rate_limited_api).merge(health));

    root.route("/ws", get(ws::handler::ws_upgrade))
        .route("/derp", get(ws::derp::derp_upgrade))
        // Security — never let a bearer-carrying query string into the logs:
        // `/ws?token=<jwt>&role=agent` and `/derp?token=<jwt>` authenticate
        // via the query (WS clients can't set headers), and TraceLayer's
        // default span records the FULL uri at DEBUG — pod logs held live
        // long-lived agent JWTs (field 2026-08-18, task #15). Queries that
        // carry no token stay logged verbatim (pagination etc. is useful).
        .layer(TraceLayer::new_for_http().make_span_with(
            |req: &axum::http::Request<axum::body::Body>| {
                let uri = req.uri();
                let shown = match uri.query() {
                    Some(q) if q.contains("token") => {
                        format!("{}?<token redacted>", uri.path())
                    }
                    _ => uri.to_string(),
                };
                tracing::debug_span!("request", method = %req.method(), uri = %shown)
            },
        ))
        .layer(cors)
        .with_state(state)
}

async fn health_check() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        // FR-69 — the compiled module set; a profile boot smoke asserts it.
        "modules": roomler_core::graph::MODULES,
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

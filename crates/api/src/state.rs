use bson::oid::ObjectId;
use dashmap::DashMap;
use mongodb::Database;
use roomler_ai_config::Settings;
use roomler_ai_remote_control::{
    Hub, audit::AuditSink, hub::ConsentEvent, models::ConsentMode, signaling::ServerMsg,
    turn_creds::TurnConfig,
};
use roomler_ai_services::{
    AuthService, EmailService, GiphyService, OAuthService, PushService, RecognitionService,
    TaskService,
    dao::{
        activation_code::ActivationCodeDao, agent::AgentDao, consent_request::ConsentRequestDao,
        file::FileDao, invite::InviteDao, message::MessageDao, notification::NotificationDao,
        overlay_network::OverlayNetworkDao, overlay_node::OverlayNodeDao,
        push_subscription::PushSubscriptionDao, reaction::ReactionDao, recording::RecordingDao,
        remote_audit::RemoteAuditDao, remote_session::RemoteSessionDao, role::RoleDao,
        room::RoomDao, tenant::TenantDao, tunnel_audit::TunnelAuditDao,
        tunnel_client::TunnelClientDao, tunnel_policy::TunnelPolicyDao, user::UserDao,
    },
    media::{room_manager::RoomManager, worker_pool::WorkerPool},
};
use tokio::sync::mpsc;

use std::sync::Arc;

use crate::ws::redis_pubsub::RedisPubSub;
use crate::ws::storage::WsStorage;

/// Outbound channel for a connected `roomler-tunnel` client, keyed by
/// the `tunnel_session_id` issued on `rc:tunnel.open`. The tunnel WS
/// handler registers its sender on TunnelOpen success and unregisters
/// on disconnect / TunnelTerminate; the agent WS handler reads this
/// map to relay `TcpForwardAccept` / `TcpForwardReject` /
/// `TcpHalfClose` / `TcpClosed` / `TunnelTerminate` from agent →
/// client.
///
/// Mirror of the Hub's per-agent `tx` registry, but kept in `AppState`
/// rather than in the `remote_control::Hub` because the Hub is the
/// remote-control session state machine and tunnel-clients are a
/// distinct lifecycle.
pub type TunnelClientOutbound = Arc<DashMap<ObjectId, mpsc::Sender<ServerMsg>>>;

#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub settings: Settings,
    pub auth: Arc<AuthService>,
    pub users: Arc<UserDao>,
    pub activation_codes: Arc<ActivationCodeDao>,
    pub tenants: Arc<TenantDao>,
    pub rooms: Arc<RoomDao>,
    pub invites: Arc<InviteDao>,
    pub messages: Arc<MessageDao>,
    pub notifications: Arc<NotificationDao>,
    pub reactions: Arc<ReactionDao>,
    pub roles: Arc<RoleDao>,
    pub files: Arc<FileDao>,
    pub recordings: Arc<RecordingDao>,

    pub tasks: Arc<TaskService>,
    pub room_manager: Arc<RoomManager>,
    pub ws_storage: Arc<WsStorage>,
    pub recognition: RecognitionService,
    pub oauth: Option<Arc<OAuthService>>,
    pub giphy: Option<Arc<GiphyService>>,
    pub email: Option<Arc<EmailService>>,
    pub push: Option<Arc<PushService>>,
    pub push_subscriptions: Arc<PushSubscriptionDao>,
    pub redis_pubsub: Option<Arc<RedisPubSub>>,

    // Remote-control subsystem
    pub agents: Arc<AgentDao>,
    pub remote_sessions: Arc<RemoteSessionDao>,
    pub remote_audit: Arc<RemoteAuditDao>,
    pub agent_crashes: Arc<roomler_ai_services::dao::agent_crash::AgentCrashDao>,
    pub agent_logs: Arc<roomler_ai_services::dao::agent_log::AgentLogDao>,
    /// Phase 4 — owner-side consent requests (email/push approve-link tokens).
    pub consent_requests: Arc<ConsentRequestDao>,
    pub rc_hub: Arc<Hub>,

    // roomler-tunnel subsystem
    pub tunnel_clients: Arc<TunnelClientDao>,
    pub tunnel_policies: Arc<TunnelPolicyDao>,
    pub tunnel_audit: Arc<TunnelAuditDao>,
    /// Per-tunnel-session WS outbound channels. See [`TunnelClientOutbound`].
    pub tunnel_clients_by_session: TunnelClientOutbound,

    // Overlay-network subsystem (Tailscale-style L3 mesh)
    pub overlay_networks: Arc<OverlayNetworkDao>,
    pub overlay_nodes: Arc<OverlayNodeDao>,
    /// Connection-lifetime WS outbound channels for **tunnel-client**
    /// overlay nodes, keyed by `tunnel_client_id` (agent nodes are
    /// reached via [`Hub::send_to_agent`]). Used by the overlay broker
    /// to fan netmaps/deltas to client nodes. Distinct from
    /// `tunnel_clients_by_session`, which is keyed per forward-session.
    pub overlay_nodes_by_id: TunnelClientOutbound,
    /// DERP relay registry: `(network_id, wg_pubkey)` → the outbound frame
    /// sender for that node's live `/derp` WS. The pubkey-addressed forwarding
    /// map for the both-UDP-blocked carrier tier. See [`crate::ws::derp`].
    pub derp_registry: crate::ws::derp::DerpRegistry,

    /// 1h-TTL in-memory cache backing `/api/agent/latest-release`.
    /// All agents share this single cache; one upstream GitHub fetch
    /// per hour vs N-agents-each-once-per-cycle. See
    /// `routes::agent_release` for the lifecycle.
    pub latest_release_cache: Arc<crate::routes::agent_release::LatestReleaseCache>,
    /// 1h-TTL in-memory cache backing `/api/tunnel/latest-release` +
    /// `/api/tunnel/installer/{platform}`. Same lifecycle as the
    /// agent cache, separate instance so the two namespaces don't
    /// share their fetched payload (different tag prefixes).
    pub tunnel_release_cache: Arc<crate::routes::tunnel_release::LatestTunnelReleaseCache>,
    /// 1h-TTL in-memory cache backing the `/api/setup/*` family (the
    /// unified roomler-setup wizard; the mixed release list is
    /// filtered to `setup-v*` per request — see
    /// `routes::setup_release`). Separate from `tunnel_release_cache`
    /// so wizard tags don't pollute the CLI's `tunnel-v*` lookups.
    /// Same lifecycle as the agent + CLI caches.
    pub setup_release_cache: Arc<crate::routes::setup_release::LatestSetupReleaseCache>,
}

impl AppState {
    pub async fn new(db: Database, settings: Settings) -> anyhow::Result<Self> {
        let auth = Arc::new(AuthService::new(settings.jwt.clone()));
        let users = Arc::new(UserDao::new(&db));
        let activation_codes = Arc::new(ActivationCodeDao::new(&db));
        let tenants = Arc::new(TenantDao::new(&db));
        let rooms = Arc::new(RoomDao::new(&db));
        let invites = Arc::new(InviteDao::new(&db));
        let messages = Arc::new(MessageDao::new(&db));
        let notifications = Arc::new(NotificationDao::new(&db));
        let reactions = Arc::new(ReactionDao::new(&db));
        let roles = Arc::new(RoleDao::new(&db));
        let files = Arc::new(FileDao::new(&db));
        let recordings = Arc::new(RecordingDao::new(&db));
        let tasks = Arc::new(TaskService::new(&db));

        let worker_pool = Arc::new(WorkerPool::new(&settings.mediasoup).await?);
        let room_manager = Arc::new(RoomManager::new(worker_pool, &settings.mediasoup));

        let ws_storage = Arc::new(WsStorage::new());
        let recognition = RecognitionService::new(
            settings.claude.api_key.clone(),
            settings.claude.model.clone(),
            settings.claude.max_tokens,
        );

        let oauth = if !settings.oauth.google.client_id.is_empty()
            || !settings.oauth.facebook.client_id.is_empty()
            || !settings.oauth.github.client_id.is_empty()
            || !settings.oauth.linkedin.client_id.is_empty()
            || !settings.oauth.microsoft.client_id.is_empty()
        {
            Some(Arc::new(OAuthService::new(settings.oauth.clone())))
        } else {
            None
        };

        // `from_settings` picks SendGrid when `email.api_key` is set
        // (prod), SMTP when `email.smtp_host` + `email.smtp_port` are
        // set (e2e Mailpit), or returns None otherwise (dev / no email).
        let email = EmailService::from_settings(&settings.email).map(Arc::new);

        let push_subscriptions = Arc::new(PushSubscriptionDao::new(&db));
        let push = if !settings.push.vapid_private_key.is_empty() {
            match PushService::new(
                &settings.push.vapid_private_key,
                settings.push.contact.clone(),
            ) {
                Ok(svc) => Some(Arc::new(svc)),
                Err(e) => {
                    tracing::warn!("Failed to initialize push service: {}", e);
                    None
                }
            }
        } else {
            None
        };

        let redis_pubsub = match RedisPubSub::new(&settings.redis.url).await {
            Ok(ps) => Some(Arc::new(ps)),
            Err(e) => {
                tracing::warn!(
                    "Failed to initialize Redis Pub/Sub: {} — cross-instance WS delivery disabled",
                    e
                );
                None
            }
        };

        let giphy = if !settings.giphy.api_key.is_empty() {
            Some(Arc::new(GiphyService::new(settings.giphy.api_key.clone())))
        } else {
            None
        };

        // Remote-control subsystem
        let agents = Arc::new(AgentDao::new(&db));
        let remote_sessions = Arc::new(RemoteSessionDao::new(&db));
        let remote_audit = Arc::new(RemoteAuditDao::new(&db));
        let agent_crashes = Arc::new(roomler_ai_services::dao::agent_crash::AgentCrashDao::new(
            &db,
        ));
        let agent_logs = Arc::new(roomler_ai_services::dao::agent_log::AgentLogDao::new(&db));

        let consent_requests = Arc::new(ConsentRequestDao::new(&db));

        let turn_cfg = build_turn_config(&settings.turn);
        let (audit_sink, _audit_handle) = AuditSink::spawn(db.clone());
        // Phase 4 — owner-side consent: the Hub emits a `ConsentEvent` for each
        // Email/Push session; this consumer resolves the owner + persists a
        // `ConsentRequest` + sends the email / web-push. Wiring `Some(consent_tx)`
        // is what turns those modes on; with `None` (tests) they'd just time out.
        let (consent_tx, consent_rx) = mpsc::channel::<ConsentEvent>(64);
        let rc_hub = Arc::new(Hub::new_with_consent(
            audit_sink,
            turn_cfg,
            Some(consent_tx),
        ));
        spawn_consent_consumer(
            consent_rx,
            ConsentConsumerDeps {
                agents: agents.clone(),
                users: users.clone(),
                consent_requests: consent_requests.clone(),
                push_subscriptions: push_subscriptions.clone(),
                email: email.clone(),
                push: push.clone(),
                base_url: settings.oauth.base_url.clone(),
            },
        );

        // roomler-tunnel subsystem
        let tunnel_clients = Arc::new(TunnelClientDao::new(&db));
        let tunnel_policies = Arc::new(TunnelPolicyDao::new(&db));
        let tunnel_audit = Arc::new(TunnelAuditDao::new(&db));

        // Overlay-network subsystem
        let overlay_networks = Arc::new(OverlayNetworkDao::new(&db));
        let overlay_nodes = Arc::new(OverlayNodeDao::new(&db));

        Ok(Self {
            db,
            settings,
            auth,
            users,
            activation_codes,
            tenants,
            rooms,
            invites,
            messages,
            notifications,
            reactions,
            roles,
            files,
            recordings,

            tasks,
            room_manager,
            ws_storage,
            recognition,
            oauth,
            giphy,
            email,
            push,
            push_subscriptions,
            redis_pubsub,
            agents,
            remote_sessions,
            remote_audit,
            agent_crashes,
            agent_logs,
            consent_requests,
            rc_hub,
            tunnel_clients,
            tunnel_policies,
            tunnel_audit,
            tunnel_clients_by_session: Arc::new(DashMap::new()),
            overlay_networks,
            overlay_nodes,
            overlay_nodes_by_id: Arc::new(DashMap::new()),
            derp_registry: Arc::new(DashMap::new()),
            latest_release_cache: crate::routes::agent_release::LatestReleaseCache::new(),
            tunnel_release_cache: crate::routes::tunnel_release::LatestTunnelReleaseCache::new(),
            setup_release_cache: crate::routes::setup_release::LatestSetupReleaseCache::new(),
        })
    }
}

/// Build a [`TurnConfig`] from settings. Returns `None` when `shared_secret` is
/// absent (e.g. dev environments using static username/password instead).
/// `pub(crate)` so the tunnel WS handler (`ws/tunnel.rs`) can mint
/// per-session QUIC-over-TURN creds the same way (Phase 3c).
pub(crate) fn build_turn_config(turn: &roomler_ai_config::TurnSettings) -> Option<TurnConfig> {
    let secret = turn.shared_secret.as_ref()?.clone();
    let base = turn.url.as_deref()?;

    // Same-worker TURN affinity (2026-07-14): optional comma-separated
    // per-worker base URLs, each expanded into the same transport variants
    // as the generic hostname. The Hub then pins BOTH sides of a session to
    // one worker (see `turn_creds::issue_for_session`) — the generic
    // hostname is 3 DNS A records, so without this each ICE side resolves
    // independently and relay↔relay sessions straddle two coturn workers.
    // Unset → empty → exactly the old single-hostname behaviour.
    let workers: Vec<Vec<String>> = turn
        .worker_urls
        .as_deref()
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|w| !w.is_empty())
                .map(expand_turn_url)
                .collect()
        })
        .unwrap_or_default();

    Some(TurnConfig {
        urls: expand_turn_url(base),
        workers,
        shared_secret: secret,
        ttl_secs: 600, // 10 minutes
    })
}

/// Expand a single `turn:host:port` base into UDP/TCP/TLS variants the same
/// way `ws/handler.rs::handle_media_join` already does for the media path, so
/// the remote-control path behaves consistently behind NAT. Factored out of
/// `build_turn_config` so the per-worker affinity URLs get the identical
/// expansion.
fn expand_turn_url(base: &str) -> Vec<String> {
    let mut urls = vec![base.to_string()];
    if base.starts_with("turn:") && !base.contains("?transport=") {
        // Plain TURN-over-UDP on :443 — same code path as the base URL, just
        // a different port. webrtc-rs's ICE agent IS able to use this; many
        // corporate firewalls drop UDP/3478 but allow UDP/443 (it looks like
        // QUIC). Requires coturn `alt-listening-port=443`.
        let turn_443 = base.replace(":3478", ":443");
        urls.push(format!("{}?transport=udp", turn_443));
        urls.push(format!("{}?transport=tcp", base));
        let turns_5349 = base
            .replacen("turn:", "turns:", 1)
            .replace(":3478", ":5349");
        urls.push(format!("{}?transport=tcp", turns_5349));
        // TURNS on :443 — both DTLS-over-UDP and TLS-over-TCP, sharing the
        // same ephemeral secret. webrtc-rs's ICE agent silently drops these
        // (TODO upstream, closed NOT_PLANNED per webrtc-rs/webrtc#690), but
        // Chrome / Firefox / Safari DO implement them, so we keep emitting
        // them for the browser-controller path.
        let turns_443 = base.replacen("turn:", "turns:", 1).replace(":3478", ":443");
        urls.push(format!("{}?transport=udp", turns_443));
        urls.push(format!("{}?transport=tcp", turns_443));
    }
    urls
}

/// Dependencies the Phase-4 owner-consent consumer needs — cheap `Arc` clones of
/// the relevant DAOs / services, captured when [`AppState`] is built.
struct ConsentConsumerDeps {
    agents: Arc<AgentDao>,
    users: Arc<UserDao>,
    consent_requests: Arc<ConsentRequestDao>,
    push_subscriptions: Arc<PushSubscriptionDao>,
    email: Option<Arc<EmailService>>,
    push: Option<Arc<PushService>>,
    base_url: String,
}

/// Spawn the background task that turns Hub [`ConsentEvent`]s (Email/Push sessions
/// awaiting the device owner) into a `ConsentRequest` row + an email / web-push
/// carrying the approve-link. One task for the process lifetime; a per-event
/// failure is logged, never fatal.
fn spawn_consent_consumer(mut rx: mpsc::Receiver<ConsentEvent>, deps: ConsentConsumerDeps) {
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if let Err(e) = handle_consent_event(&deps, &ev).await {
                tracing::warn!(session = %ev.session_id, %e, "owner-consent notification failed");
            }
        }
    });
}

async fn handle_consent_event(deps: &ConsentConsumerDeps, ev: &ConsentEvent) -> anyhow::Result<()> {
    // Resolve the device owner + display name (the Hub is DB-agnostic, so it
    // only knows the agent_id).
    let agent = deps.agents.base.find_by_id(ev.agent_id).await?;
    let owner_id = agent.owner_user_id;
    let device_name = agent.name.clone();

    // Phase 5 — break-glass NOTICE: an admin already forced the session, so this
    // is informational (no approval, no ConsentRequest). Tell the owner their
    // device was accessed + why, then we're done.
    if let Some(reason) = &ev.override_reason {
        if let Some(email) = &deps.email {
            let owner = deps.users.base.find_by_id(owner_id).await?;
            let _ = email
                .send_override_notice(&owner.email, &ev.controller_name, &device_name, reason)
                .await;
        }
        if let Some(push) = &deps.push {
            let subs = deps
                .push_subscriptions
                .find_by_user(owner_id)
                .await
                .unwrap_or_default();
            let body = format!(
                "{} accessed {} via admin break-glass. Reason: {}",
                ev.controller_name, device_name, reason
            );
            for sub in subs {
                let _ = push
                    .send(
                        &sub.endpoint,
                        &sub.keys.auth,
                        &sub.keys.p256dh,
                        "Device accessed (admin override)",
                        &body,
                        None,
                    )
                    .await;
            }
        }
        return Ok(());
    }

    // Persist the request with a fresh capability token + a TTL that matches the
    // session's consent window (a stale link can't resolve a long-gone session).
    let req = deps
        .consent_requests
        .create(
            ev.tenant_id,
            ev.session_id,
            ev.agent_id,
            ev.controller_user_id,
            ev.controller_name.clone(),
            owner_id,
            ev.timeout_secs as i64,
        )
        .await?;

    let consent_url = format!(
        "{}/consent/{}",
        deps.base_url.trim_end_matches('/'),
        req.token
    );

    match ev.mode {
        // Email + PromptThenEmail both email the owner an approve-link. For
        // PromptThenEmail the agent ALSO prompts on the host in parallel — either
        // the person at the console or the owner via the link can approve, first
        // wins (both resolve the same slot within the shared timeout).
        ConsentMode::Email | ConsentMode::PromptThenEmail => {
            let owner = deps.users.base.find_by_id(owner_id).await?;
            match &deps.email {
                Some(email) => {
                    email
                        .send_consent_request(
                            &owner.email,
                            &ev.controller_name,
                            &device_name,
                            &consent_url,
                        )
                        .await?;
                }
                None => tracing::warn!(
                    session = %ev.session_id,
                    "Email consent mode but no email service is configured — owner cannot approve"
                ),
            }
        }
        ConsentMode::Push => match &deps.push {
            Some(push) => {
                let subs = deps.push_subscriptions.find_by_user(owner_id).await?;
                if subs.is_empty() {
                    tracing::warn!(
                        session = %ev.session_id,
                        "Push consent mode but the owner has no push subscriptions"
                    );
                }
                let title = "Remote control request";
                let body = format!("{} wants to control {}", ev.controller_name, device_name);
                for sub in subs {
                    // Best-effort per subscription (a stale endpoint shouldn't
                    // block the others).
                    let _ = push
                        .send(
                            &sub.endpoint,
                            &sub.keys.auth,
                            &sub.keys.p256dh,
                            title,
                            &body,
                            Some(&consent_url),
                        )
                        .await;
                }
            }
            None => tracing::warn!(
                session = %ev.session_id,
                "Push consent mode but no push service is configured — owner cannot approve"
            ),
        },
        // The Hub only emits events for Email/Push; other modes never reach here.
        _ => {}
    }

    Ok(())
}

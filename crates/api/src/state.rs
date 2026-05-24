use bson::oid::ObjectId;
use dashmap::DashMap;
use mongodb::Database;
use roomler_ai_config::Settings;
use roomler_ai_remote_control::{
    Hub, audit::AuditSink, signaling::ServerMsg, turn_creds::TurnConfig,
};
use roomler_ai_services::{
    AuthService, EmailService, GiphyService, OAuthService, PushService, RecognitionService,
    TaskService,
    dao::{
        activation_code::ActivationCodeDao, agent::AgentDao, file::FileDao, invite::InviteDao,
        message::MessageDao, notification::NotificationDao, push_subscription::PushSubscriptionDao,
        reaction::ReactionDao, recording::RecordingDao, remote_audit::RemoteAuditDao,
        remote_session::RemoteSessionDao, role::RoleDao, room::RoomDao, tenant::TenantDao,
        tunnel_audit::TunnelAuditDao, tunnel_client::TunnelClientDao,
        tunnel_policy::TunnelPolicyDao, user::UserDao,
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
    pub rc_hub: Arc<Hub>,

    // roomler-tunnel subsystem
    pub tunnel_clients: Arc<TunnelClientDao>,
    pub tunnel_policies: Arc<TunnelPolicyDao>,
    pub tunnel_audit: Arc<TunnelAuditDao>,
    /// Per-tunnel-session WS outbound channels. See [`TunnelClientOutbound`].
    pub tunnel_clients_by_session: TunnelClientOutbound,

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
    /// 1h-TTL in-memory cache backing `/api/tunnel-wizard/{latest-release,
    /// installer/{platform}}`. Separate from `tunnel_release_cache` so the
    /// wizard's `tunnel-wizard-v*` tags don't pollute the CLI's
    /// `tunnel-v*` lookups. Same lifecycle as the agent + CLI caches.
    pub tunnel_wizard_release_cache:
        Arc<crate::routes::tunnel_wizard_release::LatestTunnelWizardReleaseCache>,
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

        let turn_cfg = build_turn_config(&settings.turn);
        let (audit_sink, _audit_handle) = AuditSink::spawn(db.clone());
        let rc_hub = Arc::new(Hub::new(audit_sink, turn_cfg));

        // roomler-tunnel subsystem
        let tunnel_clients = Arc::new(TunnelClientDao::new(&db));
        let tunnel_policies = Arc::new(TunnelPolicyDao::new(&db));
        let tunnel_audit = Arc::new(TunnelAuditDao::new(&db));

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
            rc_hub,
            tunnel_clients,
            tunnel_policies,
            tunnel_audit,
            tunnel_clients_by_session: Arc::new(DashMap::new()),
            latest_release_cache: crate::routes::agent_release::LatestReleaseCache::new(),
            tunnel_release_cache: crate::routes::tunnel_release::LatestTunnelReleaseCache::new(),
            tunnel_wizard_release_cache:
                crate::routes::tunnel_wizard_release::LatestTunnelWizardReleaseCache::new(),
        })
    }
}

/// Build a [`TurnConfig`] from settings. Returns `None` when `shared_secret` is
/// absent (e.g. dev environments using static username/password instead).
fn build_turn_config(turn: &roomler_ai_config::TurnSettings) -> Option<TurnConfig> {
    let secret = turn.shared_secret.as_ref()?.clone();
    let base = turn.url.as_deref()?;

    // Expand a single `turn:host:port` into UDP/TCP/TLS variants the same way
    // `ws/handler.rs::handle_media_join` already does for the media path, so
    // the remote-control path behaves consistently behind NAT.
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

    Some(TurnConfig {
        urls,
        shared_secret: secret,
        ttl_secs: 600, // 10 minutes
    })
}

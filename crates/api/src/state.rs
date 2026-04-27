use mongodb::Database;
use roomler_ai_config::Settings;
use roomler_ai_remote_control::{Hub, audit::AuditSink, turn_creds::TurnConfig};
use roomler_ai_services::{
    AuthService, EmailService, GiphyService, OAuthService, PushService, RecognitionService,
    TaskService,
    dao::{
        activation_code::ActivationCodeDao, agent::AgentDao, file::FileDao, invite::InviteDao,
        message::MessageDao, notification::NotificationDao, push_subscription::PushSubscriptionDao,
        reaction::ReactionDao, recording::RecordingDao, remote_audit::RemoteAuditDao,
        remote_session::RemoteSessionDao, role::RoleDao, room::RoomDao, tenant::TenantDao,
        user::UserDao,
    },
    media::{room_manager::RoomManager, worker_pool::WorkerPool},
};

use std::sync::Arc;

use crate::ws::redis_pubsub::RedisPubSub;
use crate::ws::storage::WsStorage;

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
    pub rc_hub: Arc<Hub>,

    /// 1h-TTL in-memory cache backing `/api/agent/latest-release`.
    /// All agents share this single cache; one upstream GitHub fetch
    /// per hour vs N-agents-each-once-per-cycle. See
    /// `routes::agent_release` for the lifecycle.
    pub latest_release_cache: Arc<crate::routes::agent_release::LatestReleaseCache>,
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

        let email = if !settings.email.api_key.is_empty() {
            Some(Arc::new(EmailService::new(
                settings.email.api_key.clone(),
                settings.email.from_email.clone(),
                settings.email.from_name.clone(),
            )))
        } else {
            None
        };

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

        let turn_cfg = build_turn_config(&settings.turn);
        let (audit_sink, _audit_handle) = AuditSink::spawn(db.clone());
        let rc_hub = Arc::new(Hub::new(audit_sink, turn_cfg));

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
            rc_hub,
            latest_release_cache: crate::routes::agent_release::LatestReleaseCache::new(),
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
        urls.push(format!("{}?transport=tcp", base));
        let turns_5349 = base
            .replacen("turn:", "turns:", 1)
            .replace(":3478", ":5349");
        urls.push(format!("{}?transport=tcp", turns_5349));
        // TURNS on :443 — both DTLS-over-UDP and TLS-over-TCP,
        // sharing the same ephemeral secret. :443 is the only
        // outbound port many enterprise firewalls allow; advertising
        // both transports lets the client pick whichever survives
        // local egress rules. Relies on coturn listening on :443
        // for both DTLS (UDP) and TLS (TCP) — the tls-listening-port
        // + alt-tls-listening-port lines in coturn's config.
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

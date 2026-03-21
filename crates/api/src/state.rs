use mongodb::Database;
use roomler2_config::Settings;
use roomler2_services::{
    AuthService, EmailService, GiphyService, OAuthService, PushService, RecognitionService,
    TaskService,
    dao::{
        activation_code::ActivationCodeDao,
        file::FileDao, invite::InviteDao, message::MessageDao, notification::NotificationDao,
        push_subscription::PushSubscriptionDao, reaction::ReactionDao, recording::RecordingDao,
        role::RoleDao, room::RoomDao, tenant::TenantDao,
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
            match PushService::new(&settings.push.vapid_private_key, settings.push.contact.clone())
            {
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
                tracing::warn!("Failed to initialize Redis Pub/Sub: {} — cross-instance WS delivery disabled", e);
                None
            }
        };

        let giphy = if !settings.giphy.api_key.is_empty() {
            Some(Arc::new(GiphyService::new(
                settings.giphy.api_key.clone(),
            )))
        } else {
            None
        };

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
        })
    }
}

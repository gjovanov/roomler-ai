use mongodb::Database;
use roomler2_config::Settings;
use roomler2_services::{
    AuthService, OAuthService, RecognitionService, TaskService,
    dao::{
        channel::ChannelDao, conference::ConferenceDao, file::FileDao,
        message::MessageDao, reaction::ReactionDao, recording::RecordingDao,
        tenant::TenantDao, transcription::TranscriptionDao, user::UserDao,
    },
    media::{room_manager::RoomManager, worker_pool::WorkerPool},
};
use std::sync::Arc;

use crate::ws::storage::WsStorage;

#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub settings: Settings,
    pub auth: Arc<AuthService>,
    pub users: Arc<UserDao>,
    pub tenants: Arc<TenantDao>,
    pub channels: Arc<ChannelDao>,
    pub messages: Arc<MessageDao>,
    pub reactions: Arc<ReactionDao>,
    pub conferences: Arc<ConferenceDao>,
    pub files: Arc<FileDao>,
    pub recordings: Arc<RecordingDao>,
    pub transcriptions: Arc<TranscriptionDao>,
    pub tasks: Arc<TaskService>,
    pub room_manager: Arc<RoomManager>,
    pub ws_storage: Arc<WsStorage>,
    pub recognition: RecognitionService,
    pub oauth: Option<Arc<OAuthService>>,
}

impl AppState {
    pub async fn new(db: Database, settings: Settings) -> anyhow::Result<Self> {
        let auth = Arc::new(AuthService::new(settings.jwt.clone()));
        let users = Arc::new(UserDao::new(&db));
        let tenants = Arc::new(TenantDao::new(&db));
        let channels = Arc::new(ChannelDao::new(&db));
        let messages = Arc::new(MessageDao::new(&db));
        let reactions = Arc::new(ReactionDao::new(&db));
        let conferences = Arc::new(ConferenceDao::new(&db));
        let files = Arc::new(FileDao::new(&db));
        let recordings = Arc::new(RecordingDao::new(&db));
        let transcriptions = Arc::new(TranscriptionDao::new(&db));
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

        Ok(Self {
            db,
            settings,
            auth,
            users,
            tenants,
            channels,
            messages,
            reactions,
            conferences,
            files,
            recordings,
            transcriptions,
            tasks,
            room_manager,
            ws_storage,
            recognition,
            oauth,
        })
    }
}

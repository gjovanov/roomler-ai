use mongodb::Database;
use roomler2_config::Settings;
use roomler2_services::{
    AuthService, GiphyService, OAuthService, RecognitionService, TaskService,
    dao::{
        file::FileDao, invite::InviteDao, message::MessageDao, notification::NotificationDao,
        reaction::ReactionDao, recording::RecordingDao, role::RoleDao, room::RoomDao,
        tenant::TenantDao, transcription::TranscriptionDao, user::UserDao,
    },
    media::{room_manager::RoomManager, worker_pool::WorkerPool},
};
use roomler2_transcription::TranscriptionEngine;
use std::sync::Arc;

use crate::ws::storage::WsStorage;

#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub settings: Settings,
    pub auth: Arc<AuthService>,
    pub users: Arc<UserDao>,
    pub tenants: Arc<TenantDao>,
    pub rooms: Arc<RoomDao>,
    pub invites: Arc<InviteDao>,
    pub messages: Arc<MessageDao>,
    pub notifications: Arc<NotificationDao>,
    pub reactions: Arc<ReactionDao>,
    pub roles: Arc<RoleDao>,
    pub files: Arc<FileDao>,
    pub recordings: Arc<RecordingDao>,
    pub transcriptions: Arc<TranscriptionDao>,
    pub tasks: Arc<TaskService>,
    pub room_manager: Arc<RoomManager>,
    pub ws_storage: Arc<WsStorage>,
    pub recognition: RecognitionService,
    pub oauth: Option<Arc<OAuthService>>,
    pub giphy: Option<Arc<GiphyService>>,
    pub transcription_engine: Option<Arc<TranscriptionEngine>>,
}

impl AppState {
    pub async fn new(db: Database, settings: Settings) -> anyhow::Result<Self> {
        let auth = Arc::new(AuthService::new(settings.jwt.clone()));
        let users = Arc::new(UserDao::new(&db));
        let tenants = Arc::new(TenantDao::new(&db));
        let rooms = Arc::new(RoomDao::new(&db));
        let invites = Arc::new(InviteDao::new(&db));
        let messages = Arc::new(MessageDao::new(&db));
        let notifications = Arc::new(NotificationDao::new(&db));
        let reactions = Arc::new(ReactionDao::new(&db));
        let roles = Arc::new(RoleDao::new(&db));
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

        let giphy = if !settings.giphy.api_key.is_empty() {
            Some(Arc::new(GiphyService::new(
                settings.giphy.api_key.clone(),
            )))
        } else {
            None
        };

        // Initialize transcription engine if enabled
        let transcription_engine = if settings.transcription.enabled {
            match Self::create_transcription_engine(&settings) {
                Ok((engine, _rx)) => {
                    tracing::info!(
                        backend = %settings.transcription.backend,
                        "Transcription engine initialized"
                    );
                    Some(engine)
                }
                Err(e) => {
                    tracing::warn!("Failed to initialize transcription engine: {}", e);
                    None
                }
            }
        } else {
            None
        };

        Ok(Self {
            db,
            settings,
            auth,
            users,
            tenants,
            rooms,
            invites,
            messages,
            notifications,
            reactions,
            roles,
            files,
            recordings,
            transcriptions,
            tasks,
            room_manager,
            ws_storage,
            recognition,
            oauth,
            giphy,
            transcription_engine,
        })
    }

    /// Creates the transcription engine based on settings.
    fn create_transcription_engine(
        settings: &Settings,
    ) -> anyhow::Result<(
        Arc<TranscriptionEngine>,
        tokio::sync::broadcast::Receiver<roomler2_transcription::TranscriptEvent>,
    )> {
        use roomler2_transcription::TranscriptionConfig;
        use std::collections::HashMap;
        use std::path::Path;

        let config = TranscriptionConfig {
            enabled: settings.transcription.enabled,
            backend: settings.transcription.backend.clone(),
            whisper_model_path: settings.transcription.whisper_model_path.clone(),
            language: settings.transcription.language.clone(),
            vad_model_path: settings.transcription.vad_model_path.clone(),
            vad_start_threshold: settings.transcription.vad_start_threshold,
            vad_end_threshold: settings.transcription.vad_end_threshold,
            vad_min_speech_frames: settings.transcription.vad_min_speech_frames,
            vad_min_silence_frames: settings.transcription.vad_min_silence_frames,
            vad_pre_speech_pad_frames: settings.transcription.vad_pre_speech_pad_frames,
            max_speech_duration_secs: settings.transcription.max_speech_duration_secs,
            nim_endpoint: settings.transcription.nim_endpoint.clone(),
            onnx_model_path: settings.transcription.onnx_model_path.clone(),
            nim_model: settings.transcription.nim_model.clone(),
            streaming_partial_interval_ms: settings.transcription.streaming_partial_interval_ms,
        };

        let mut backends: HashMap<String, Arc<dyn roomler2_transcription::AsrBackend>> =
            HashMap::new();

        // Local Whisper backend
        if let Some(ref path) = settings.transcription.whisper_model_path {
            if Path::new(path).exists() {
                match roomler2_transcription::asr::local_whisper::LocalWhisperBackend::new(
                    path,
                    settings.transcription.language.clone(),
                ) {
                    Ok(backend) => {
                        tracing::info!(path, "Whisper backend loaded");
                        backends.insert("whisper".into(), Arc::new(backend));
                    }
                    Err(e) => {
                        tracing::warn!(path, %e, "Failed to load Whisper backend");
                    }
                }
            } else {
                tracing::info!(path, "Whisper model file not found, skipping");
            }
        }

        // Local ONNX (Canary) backend
        if let Some(ref path) = settings.transcription.onnx_model_path {
            if Path::new(path).exists() {
                match roomler2_transcription::asr::local_onnx::LocalOnnxBackend::new(path) {
                    Ok(backend) => {
                        tracing::info!(path, "Canary ONNX backend loaded");
                        backends.insert("canary".into(), Arc::new(backend));
                    }
                    Err(e) => {
                        tracing::warn!(path, %e, "Failed to load Canary ONNX backend");
                    }
                }
            } else {
                tracing::info!(path, "Canary ONNX model directory not found, skipping");
            }
        }

        // Remote NIM backend
        #[cfg(feature = "remote-nim")]
        if let Some(ref endpoint) = settings.transcription.nim_endpoint {
            let model_name = settings.transcription.nim_model.as_deref();
            match roomler2_transcription::asr::remote_nim::RemoteNimBackend::new(endpoint, model_name) {
                Ok(backend) => {
                    tracing::info!(endpoint, ?model_name, "NIM backend configured");
                    backends.insert("nim".into(), Arc::new(backend));
                }
                Err(e) => {
                    tracing::warn!(endpoint, %e, "Failed to create NIM backend");
                }
            }
        }

        if backends.is_empty() {
            anyhow::bail!("No ASR backends available — check model paths and feature flags");
        }

        // Map config backend name to our key
        let default_backend = match settings.transcription.backend.as_str() {
            "local_whisper" => "whisper",
            "local_onnx" => "canary",
            "remote_nim" => "nim",
            other => other,
        }
        .to_string();

        let (engine, rx) = TranscriptionEngine::new(backends, default_backend, config);
        Ok((engine, rx))
    }
}

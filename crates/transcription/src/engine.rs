use std::collections::HashMap;
use std::sync::Arc;

use bson::oid::ObjectId;
use dashmap::{DashMap, DashSet};
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::{debug, info, warn};

use crate::asr::AsrBackend;
use crate::config::TranscriptionConfig;
use crate::file_playback::FilePlaybackWorker;
use crate::worker::TranscriptionWorker;
use crate::TranscriptEvent;

/// Manages per-producer transcription pipelines with multi-backend support.
///
/// The engine is created once at startup and shared via `Arc`. It supports
/// multiple named ASR backends and per-room model selection.
pub struct TranscriptionEngine {
    /// Named ASR backends (e.g. "whisper" -> LocalWhisperBackend, "canary" -> LocalOnnxBackend).
    backends: HashMap<String, Arc<dyn AsrBackend>>,
    /// Default backend name.
    default_backend: String,
    config: TranscriptionConfig,
    /// Active worker tasks, keyed by producer_id string.
    workers: DashMap<String, WorkerHandle>,
    /// Broadcast channel for transcript events.
    transcript_tx: broadcast::Sender<TranscriptEvent>,
    /// Per-room model selection: room_id -> backend name.
    room_models: Mutex<HashMap<ObjectId, String>>,
    /// Conferences with an active broadcast task (prevents spawning duplicates).
    broadcast_active: DashSet<ObjectId>,
    /// Tracks which WS connection started which playback IDs (for cleanup on disconnect).
    connection_playbacks: DashMap<String, Vec<String>>,
}

struct WorkerHandle {
    abort_handle: tokio::task::AbortHandle,
}

impl TranscriptionEngine {
    /// Creates a new multi-backend transcription engine.
    ///
    /// Returns `(engine, transcript_receiver)`.
    pub fn new(
        backends: HashMap<String, Arc<dyn AsrBackend>>,
        default_backend: String,
        config: TranscriptionConfig,
    ) -> (Arc<Self>, broadcast::Receiver<TranscriptEvent>) {
        let (transcript_tx, transcript_rx) = broadcast::channel(256);

        let backend_names: Vec<&String> = backends.keys().collect();
        info!(
            ?backend_names,
            default = %default_backend,
            "Transcription engine created"
        );

        let engine = Arc::new(Self {
            backends,
            default_backend,
            config,
            workers: DashMap::new(),
            transcript_tx,
            room_models: Mutex::new(HashMap::new()),
            broadcast_active: DashSet::new(),
            connection_playbacks: DashMap::new(),
        });

        (engine, transcript_rx)
    }

    /// Returns a new broadcast receiver for transcript events.
    pub fn subscribe(&self) -> broadcast::Receiver<TranscriptEvent> {
        self.transcript_tx.subscribe()
    }

    /// Returns the default backend name.
    pub fn default_backend_name(&self) -> &str {
        &self.default_backend
    }

    /// Attempts to mark a room as having an active broadcast task.
    ///
    /// Returns `true` if the conference was NOT already tracked (caller should
    /// spawn the task). Returns `false` if a broadcast task already exists
    /// (caller should skip).
    pub fn try_start_broadcast(&self, room_id: ObjectId) -> bool {
        self.broadcast_active.insert(room_id)
    }

    /// Removes the broadcast-active flag for a room (called when the
    /// broadcast task exits).
    pub fn clear_broadcast(&self, room_id: &ObjectId) {
        self.broadcast_active.remove(room_id);
    }

    /// Enables transcription for a room with a specific model.
    pub async fn enable_room(&self, room_id: ObjectId, model_name: String) {
        let mut models = self.room_models.lock().await;
        models.insert(room_id, model_name.clone());
        info!(%room_id, model = %model_name, "Transcription enabled for room");
    }

    /// Disables transcription for a room and stops all its workers.
    pub async fn disable_room(&self, room_id: ObjectId) {
        {
            let mut models = self.room_models.lock().await;
            models.remove(&room_id);
        }

        // Stop all workers for this room (live producers and file playbacks)
        let hex = room_id.to_hex();
        let to_remove: Vec<String> = self
            .workers
            .iter()
            .filter(|entry| {
                let key = entry.key();
                key.starts_with(&hex) || key.starts_with(&format!("file:{}", hex))
            })
            .map(|entry| entry.key().clone())
            .collect();

        for key in to_remove {
            self.stop_pipeline(&key);
        }

        // Clear broadcast flag so the task exits on next recv() check
        self.clear_broadcast(&room_id);

        info!(%room_id, "Transcription disabled for room");
    }

    /// Checks if transcription is enabled for a room.
    pub async fn is_enabled(&self, room_id: &ObjectId) -> bool {
        let models = self.room_models.lock().await;
        models.contains_key(room_id)
    }

    /// Gets the ASR backend for a room, falling back to default.
    fn get_backend(&self, room_id: &ObjectId, models: &HashMap<ObjectId, String>) -> Option<Arc<dyn AsrBackend>> {
        let model_name = models
            .get(room_id)
            .unwrap_or(&self.default_backend);

        if let Some(backend) = self.backends.get(model_name) {
            return Some(backend.clone());
        }

        // Fallback: try default backend
        if let Some(backend) = self.backends.get(&self.default_backend) {
            warn!(
                requested = %model_name,
                fallback = %self.default_backend,
                "Requested backend not found, using default"
            );
            return Some(backend.clone());
        }

        // Fallback: try any available backend
        if let Some((name, backend)) = self.backends.iter().next() {
            warn!(fallback = %name, "Default backend not found, using first available");
            return Some(backend.clone());
        }

        None
    }

    /// Starts a transcription pipeline for an audio producer.
    ///
    /// If a pipeline already exists for this producer, it is stopped first
    /// (handles model switching).
    pub fn start_pipeline(
        self: &Arc<Self>,
        room_id: ObjectId,
        producer_id: String,
        user_id: ObjectId,
        speaker_name: String,
        rtp_rx: mpsc::Receiver<Vec<u8>>,
    ) {
        let key = format!("{}:{}", room_id.to_hex(), producer_id);

        // Stop any existing pipeline for this producer (e.g. model switch)
        if self.workers.contains_key(&key) {
            info!(%key, "Replacing existing pipeline (model switch)");
            self.stop_pipeline(&key);
        }

        // Resolve backend synchronously using try_lock
        let asr = {
            let models = match self.room_models.try_lock() {
                Ok(guard) => guard,
                Err(_) => {
                    // If we can't lock, use default backend directly
                    match self.backends.get(&self.default_backend)
                        .or_else(|| self.backends.values().next())
                        .cloned()
                    {
                        Some(backend) => {
                            self.spawn_worker(key, room_id, producer_id, user_id, speaker_name, backend, rtp_rx);
                            return;
                        }
                        None => {
                            warn!("No ASR backends available");
                            return;
                        }
                    }
                }
            };
            match self.get_backend(&room_id, &models) {
                Some(b) => b,
                None => {
                    warn!("No ASR backends available for room {}", room_id);
                    return;
                }
            }
        };

        self.spawn_worker(key, room_id, producer_id, user_id, speaker_name, asr, rtp_rx);
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_worker(
        self: &Arc<Self>,
        key: String,
        room_id: ObjectId,
        _producer_id: String,
        user_id: ObjectId,
        speaker_name: String,
        asr: Arc<dyn AsrBackend>,
        rtp_rx: mpsc::Receiver<Vec<u8>>,
    ) {
        debug!(%key, backend = %asr.name(), "Starting transcription pipeline");

        let worker = TranscriptionWorker::new(
            user_id,
            room_id,
            speaker_name.clone(),
            asr,
            self.config.clone(),
            rtp_rx,
            self.transcript_tx.clone(),
        );

        // Spawn worker and auto-cleanup on completion
        let cleanup_key = key.clone();
        let engine = Arc::clone(self);
        let handle = tokio::spawn(async move {
            worker.run().await;
            // Remove from workers map when done (natural exit or RTP channel closed)
            engine.workers.remove(&cleanup_key);
            debug!(%cleanup_key, "Worker entry cleaned up");
        });

        self.workers.insert(
            key.clone(),
            WorkerHandle {
                abort_handle: handle.abort_handle(),
            },
        );

        debug!(%key, %speaker_name, "Transcription pipeline started");
    }

    /// Stops a transcription pipeline by its key (room_id:producer_id).
    pub fn stop_pipeline(&self, key: &str) {
        if let Some((_, handle)) = self.workers.remove(key) {
            handle.abort_handle.abort();
            debug!(%key, "Transcription pipeline stopped");
        }
    }

    /// Stops the pipeline for a specific producer.
    pub fn stop_producer(&self, room_id: &ObjectId, producer_id: &str) {
        let key = format!("{}:{}", room_id.to_hex(), producer_id);
        self.stop_pipeline(&key);
    }

    /// Returns the number of active pipelines.
    pub fn active_pipeline_count(&self) -> usize {
        self.workers.len()
    }

    /// Returns the list of available backend names.
    pub fn available_backends(&self) -> Vec<String> {
        self.backends.keys().cloned().collect()
    }

    /// Tracks a playback ID as belonging to a WS connection (for cleanup on disconnect).
    pub fn track_playback(&self, connection_id: &str, playback_id: &str) {
        self.connection_playbacks
            .entry(connection_id.to_string())
            .or_default()
            .push(playback_id.to_string());
    }

    /// Stops all playback workers started by a given WS connection.
    pub fn stop_connection_playbacks(&self, connection_id: &str) {
        if let Some((_, playback_ids)) = self.connection_playbacks.remove(connection_id) {
            for pid in &playback_ids {
                self.stop_pipeline(pid);
            }
            if !playback_ids.is_empty() {
                info!(%connection_id, count = playback_ids.len(), "Stopped playbacks on disconnect");
            }
        }
    }

    /// Starts a file playback pipeline that reads a WAV file through VAD → ASR.
    ///
    /// Returns a playback ID (used to stop it later) or None if no backend is available.
    pub async fn start_file_playback(
        self: &Arc<Self>,
        room_id: ObjectId,
        file_path: String,
        user_id: ObjectId,
        speaker_name: String,
    ) -> Option<String> {
        let asr = {
            let models = self.room_models.lock().await;
            self.get_backend(&room_id, &models)?
        };

        let playback_id = format!(
            "file:{}:{}",
            room_id.to_hex(),
            bson::Uuid::new()
        );
        let key = playback_id.clone();

        let worker = FilePlaybackWorker::new(
            room_id,
            user_id,
            speaker_name,
            file_path,
            asr,
            self.config.clone(),
            self.transcript_tx.clone(),
        );

        let cleanup_key = key.clone();
        let engine = Arc::clone(self);
        let handle = tokio::spawn(async move {
            worker.run().await;
            engine.workers.remove(&cleanup_key);
            debug!(%cleanup_key, "File playback worker cleaned up");
        });

        self.workers.insert(
            key,
            WorkerHandle {
                abort_handle: handle.abort_handle(),
            },
        );

        Some(playback_id)
    }
}

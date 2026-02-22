pub mod asr;
pub mod config;
pub mod engine;
pub mod file_playback;
pub mod pipeline;
#[cfg(feature = "vad")]
pub mod vad;
pub mod wer;
pub mod worker;

pub use asr::{AsrBackend, AsrRequest, StreamingAsrBackend, StreamingConfig, StreamingResult, TranscriptionResult};
pub use config::TranscriptionConfig;
pub use engine::TranscriptionEngine;

use bson::oid::ObjectId;
use serde::{Deserialize, Serialize};

/// A transcription event emitted when an utterance is transcribed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEvent {
    pub room_id: ObjectId,
    pub user_id: ObjectId,
    pub speaker_name: String,
    pub text: String,
    pub language: Option<String>,
    pub confidence: Option<f64>,
    /// Seconds since room transcription started.
    pub start_time: f64,
    /// Seconds since room transcription started.
    pub end_time: f64,
    /// How long ASR inference took in milliseconds.
    pub inference_duration_ms: u64,
    /// Whether this is a final transcript or a partial (growing) result.
    pub is_final: bool,
    /// Stable ID for correlating PARTIAL updates with their FINAL replacement.
    /// Format: `"{room_hex}:{speaker_hex}:{utterance_start_time}"`.
    pub segment_id: String,
}

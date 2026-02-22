#[cfg(feature = "local-whisper")]
pub mod local_whisper;

#[cfg(feature = "local-onnx")]
pub mod canary;

#[cfg(feature = "local-onnx")]
pub mod local_onnx;

#[cfg(feature = "remote-nim")]
pub mod remote_nim;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Request to transcribe an audio segment.
pub struct AsrRequest {
    /// PCM audio at 16kHz mono, f32 normalized [-1.0, 1.0].
    pub audio_pcm_16k_mono: Vec<f32>,
    /// Optional language hint (ISO 639-1, e.g. "en", "de").
    pub language_hint: Option<String>,
    /// Sample rate (always 16000 for this pipeline).
    pub sample_rate: u32,
}

/// Result of an ASR transcription.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptionResult {
    pub text: String,
    pub language: Option<String>,
    pub confidence: Option<f64>,
}

/// Trait for pluggable ASR backends.
#[async_trait]
pub trait AsrBackend: Send + Sync + 'static {
    /// Transcribes a complete utterance (post-VAD).
    async fn transcribe(&self, request: AsrRequest) -> anyhow::Result<TranscriptionResult>;

    /// Human-readable backend name.
    fn name(&self) -> &str;

    /// Whether this backend supports a given language code.
    fn supports_language(&self, lang: &str) -> bool;

    /// Whether this backend supports native streaming (partial + final results).
    ///
    /// Backends that return `true` can be used with `StreamingAsrBackend` trait.
    fn supports_streaming(&self) -> bool {
        false
    }
}

/// Configuration for a streaming ASR session.
pub struct StreamingConfig {
    /// Optional language hint (ISO 639-1, e.g. "en", "de").
    pub language_hint: Option<String>,
    /// Sample rate (always 16000 for this pipeline).
    pub sample_rate: u32,
}

/// A streaming recognition result (partial or final).
#[derive(Debug, Clone)]
pub struct StreamingResult {
    /// Transcribed text.
    pub text: String,
    /// Whether this is a final result or an interim/partial result.
    pub is_final: bool,
    /// Detected language (if available).
    pub language: Option<String>,
    /// Confidence score (if available).
    pub confidence: Option<f64>,
}

/// Extended trait for backends that support native streaming (partial + final results).
///
/// Backends implementing this trait can receive audio chunks incrementally and
/// produce interim (partial) results as audio arrives, followed by a final result.
#[async_trait]
pub trait StreamingAsrBackend: AsrBackend {
    /// Starts a streaming recognition session.
    ///
    /// Returns a sender for audio chunks and a receiver for streaming results.
    /// Send audio chunks via the sender; receive partial and final results via the receiver.
    /// Drop the sender to signal end of audio.
    async fn start_stream(
        &self,
        config: StreamingConfig,
    ) -> anyhow::Result<(
        tokio::sync::mpsc::Sender<Vec<f32>>,
        tokio::sync::mpsc::Receiver<StreamingResult>,
    )>;
}

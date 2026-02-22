use serde::{Deserialize, Serialize};

/// Configuration for the transcription system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptionConfig {
    /// Enable the transcription engine.
    pub enabled: bool,
    /// ASR backend to use: "local_whisper", "local_onnx", "remote_nim".
    pub backend: String,
    /// Path to the Whisper model file (for local_whisper backend).
    pub whisper_model_path: Option<String>,
    /// Language hint for ASR (e.g. "en", "de"). None = auto-detect.
    pub language: Option<String>,
    /// Path to the Silero VAD ONNX model file.
    pub vad_model_path: Option<String>,
    /// VAD speech start threshold (0.0-1.0).
    pub vad_start_threshold: f32,
    /// VAD speech end threshold (0.0-1.0).
    pub vad_end_threshold: f32,
    /// Minimum consecutive speech frames to start (at 32ms/frame).
    pub vad_min_speech_frames: usize,
    /// Minimum consecutive silence frames to end speech (at 32ms/frame).
    pub vad_min_silence_frames: usize,
    /// Pre-speech padding frames to include before detected speech start.
    pub vad_pre_speech_pad_frames: usize,
    /// Maximum speech duration in seconds before force-ending.
    pub max_speech_duration_secs: f64,
    /// NIM gRPC endpoint (for remote_nim backend).
    pub nim_endpoint: Option<String>,
    /// Path to local ONNX ASR model (for local_onnx backend).
    pub onnx_model_path: Option<String>,
    /// NIM model name (for remote_nim backend, e.g. "canary-1b" or "parakeet-ctc-1.1b").
    pub nim_model: Option<String>,
    /// Interval in milliseconds between streaming partial transcription results.
    pub streaming_partial_interval_ms: u64,
}

impl Default for TranscriptionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: "local_onnx".to_string(),
            whisper_model_path: None,
            language: None,
            vad_model_path: None,
            vad_start_threshold: 0.45,
            vad_end_threshold: 0.30,
            vad_min_speech_frames: 3,
            vad_min_silence_frames: 20,
            vad_pre_speech_pad_frames: 15,
            max_speech_duration_secs: 30.0,
            nim_endpoint: None,
            onnx_model_path: None,
            nim_model: None,
            streaming_partial_interval_ms: 500,
        }
    }
}

use std::sync::Arc;

use bson::oid::ObjectId;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::asr::{AsrBackend, AsrRequest};
use crate::config::TranscriptionConfig;
use crate::pipeline::wav_reader::read_wav_16k_mono;
use crate::TranscriptEvent;

#[cfg(feature = "vad")]
use crate::vad::{SileroVad, VadEvent};

/// VAD chunk size: 512 samples @ 16kHz = 32ms
const VAD_CHUNK_SIZE: usize = 512;

/// Processes a WAV file through VAD → ASR, emitting TranscriptEvents.
///
/// Same pipeline as the real-time TranscriptionWorker but fed from a WAV file
/// instead of an RTP stream.
pub struct FilePlaybackWorker {
    room_id: ObjectId,
    user_id: ObjectId,
    speaker_name: String,
    file_path: String,
    asr: Arc<dyn AsrBackend>,
    config: TranscriptionConfig,
    transcript_tx: broadcast::Sender<TranscriptEvent>,
}

/// A speech segment with timestamp.
struct SpeechSegment {
    audio: Vec<f32>,
    start_time: f64,
    end_time: f64,
    is_final: bool,
    segment_id: String,
}

impl FilePlaybackWorker {
    pub fn new(
        room_id: ObjectId,
        user_id: ObjectId,
        speaker_name: String,
        file_path: String,
        asr: Arc<dyn AsrBackend>,
        config: TranscriptionConfig,
        transcript_tx: broadcast::Sender<TranscriptEvent>,
    ) -> Self {
        Self {
            room_id,
            user_id,
            speaker_name,
            file_path,
            asr,
            config,
            transcript_tx,
        }
    }

    /// Runs the file playback pipeline.
    ///
    /// Reads the WAV file, processes through VAD to find speech segments,
    /// then transcribes each segment via ASR.
    pub async fn run(self) {
        info!(
            room_id = %self.room_id,
            file = %self.file_path,
            speaker = %self.speaker_name,
            "File playback worker started"
        );

        // 1. Read WAV file
        let (audio, sample_rate) = match read_wav_16k_mono(&self.file_path) {
            Ok(r) => r,
            Err(e) => {
                error!("Failed to read WAV file '{}': {}", self.file_path, e);
                return;
            }
        };

        info!(
            samples = audio.len(),
            sample_rate,
            duration_secs = audio.len() as f64 / sample_rate as f64,
            "WAV file loaded"
        );

        // 2. Run VAD to find speech segments
        let segments = self.run_vad(&audio);
        info!(segments = segments.len(), "VAD segments extracted");

        // 3. Transcribe each segment
        for (i, segment) in segments.iter().enumerate() {
            let request = AsrRequest {
                audio_pcm_16k_mono: segment.audio.clone(),
                language_hint: self.config.language.clone(),
                sample_rate: 16000,
            };

            let start = std::time::Instant::now();
            match self.asr.transcribe(request).await {
                Ok(result) => {
                    let inference_duration_ms = start.elapsed().as_millis() as u64;
                    let text = result.text.trim().to_string();
                    if text.is_empty() || is_hallucination(&text) {
                        debug!("ASR segment {} returned empty/hallucinated text: '{}', skipping", i, text);
                        continue;
                    }

                    let event = TranscriptEvent {
                        room_id: self.room_id,
                        user_id: self.user_id,
                        speaker_name: self.speaker_name.clone(),
                        text,
                        language: result.language,
                        confidence: result.confidence,
                        start_time: segment.start_time,
                        end_time: segment.end_time,
                        inference_duration_ms,
                        is_final: segment.is_final,
                        segment_id: segment.segment_id.clone(),
                    };

                    if let Err(e) = self.transcript_tx.send(event) {
                        debug!("No transcript subscribers: {}", e);
                    }
                }
                Err(e) => {
                    warn!("ASR transcription error on segment {}: {}", i, e);
                }
            }
        }

        info!(
            room_id = %self.room_id,
            "File playback worker finished"
        );
    }

    /// Processes audio through VAD, returns speech segments with timestamps.
    fn run_vad(&self, audio: &[f32]) -> Vec<SpeechSegment> {
        #[cfg(feature = "vad")]
        {
            let vad_path = self
                .config
                .vad_model_path
                .as_deref()
                .unwrap_or("models/silero_vad.onnx");
            let mut vad = match SileroVad::new(vad_path, &self.config) {
                Ok(v) => v,
                Err(e) => {
                    error!("Failed to create VAD: {}", e);
                    // Fallback: treat entire audio as one segment
                    return vec![SpeechSegment {
                        audio: audio.to_vec(),
                        start_time: 0.0,
                        end_time: audio.len() as f64 / 16000.0,
                        is_final: true,
                        segment_id: format!(
                            "{}:{}:{:.3}",
                            self.room_id.to_hex(),
                            self.user_id.to_hex(),
                            0.0,
                        ),
                    }];
                }
            };

            let mut segments = Vec::new();
            let mut sample_offset = 0usize;
            let conf_hex = self.room_id.to_hex();
            let user_hex = self.user_id.to_hex();

            for chunk in audio.chunks(VAD_CHUNK_SIZE) {
                if chunk.len() < VAD_CHUNK_SIZE {
                    break;
                }

                let events = vad.process(chunk);
                for event in events {
                    match event {
                        VadEvent::SpeechStart => {
                            // File playback only emits FINAL segments (batch mode)
                        }
                        VadEvent::SpeechEnd {
                            audio: speech_audio,
                            duration_secs,
                        } => {
                            let end_secs = sample_offset as f64 / 16000.0;
                            let start_secs = end_secs - duration_secs;
                            segments.push(SpeechSegment {
                                audio: speech_audio,
                                start_time: start_secs,
                                end_time: end_secs,
                                is_final: true,
                                segment_id: format!(
                                    "{}:{}:{:.3}",
                                    conf_hex, user_hex, start_secs,
                                ),
                            });
                        }
                    }
                }

                sample_offset += chunk.len();
            }

            // Flush: pad with silence to trigger SpeechEnd for any in-progress speech.
            // 30 chunks × 512 samples @ 16kHz ≈ 960ms of silence — enough for VAD to
            // detect end-of-speech even with the most conservative threshold.
            let silence = vec![0.0f32; VAD_CHUNK_SIZE];
            for _ in 0..30 {
                let events = vad.process(&silence);
                for event in events {
                    match event {
                        VadEvent::SpeechStart => {}
                        VadEvent::SpeechEnd {
                            audio: speech_audio,
                            duration_secs,
                        } => {
                            let end_secs = sample_offset as f64 / 16000.0;
                            let start_secs = end_secs - duration_secs;
                            segments.push(SpeechSegment {
                                audio: speech_audio,
                                start_time: start_secs,
                                end_time: end_secs,
                                is_final: true,
                                segment_id: format!(
                                    "{}:{}:{:.3}",
                                    conf_hex, user_hex, start_secs,
                                ),
                            });
                        }
                    }
                }
                sample_offset += VAD_CHUNK_SIZE;
            }

            segments
        }

        #[cfg(not(feature = "vad"))]
        {
            // Without VAD, treat entire audio as one segment
            vec![SpeechSegment {
                audio: audio.to_vec(),
                start_time: 0.0,
                end_time: audio.len() as f64 / 16000.0,
                is_final: true,
                segment_id: format!(
                    "{}:{}:{:.3}",
                    self.room_id.to_hex(),
                    self.user_id.to_hex(),
                    0.0,
                ),
            }]
        }
    }
}

/// Returns true if the text is a known Whisper hallucination/placeholder.
fn is_hallucination(text: &str) -> bool {
    let lower = text.to_lowercase();
    // Whisper hallucination markers (enclosed in brackets or repeated patterns)
    lower.contains("[blank_audio]")
        || lower.contains("[silence]")
        || lower.contains("[music]")
        || lower.contains("(silence)")
        || lower.contains("(music)")
        || lower == "you"
        || lower == "thank you."
        || lower == "thanks for watching!"
}

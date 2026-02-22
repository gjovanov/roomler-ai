use std::sync::Arc;

use bson::oid::ObjectId;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, warn};

use crate::asr::{AsrBackend, AsrRequest};
use crate::config::TranscriptionConfig;
use crate::pipeline::rtp_parser::RtpPacket;
use crate::pipeline::{OpusDecoder, Resampler};
use crate::TranscriptEvent;

#[cfg(feature = "vad")]
use crate::vad::{SileroVad, VadEvent};

/// Guard that aborts a spawned task when dropped.
///
/// `tokio::spawn` returns a `JoinHandle` whose `Drop` impl detaches (does NOT abort)
/// the task. This wrapper ensures the task is cancelled if the owning future is cancelled.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A speech segment produced by the ingestion loop for the ASR loop.
struct SpeechSegment {
    audio: Vec<f32>,
    start_time: f64,
    end_time: f64,
    is_final: bool,
    segment_id: String,
}

/// Per-producer async pipeline task.
///
/// Receives RTP packets from mediasoup DirectTransport, processes them through:
/// RTP parse → Opus decode → Resample (48kHz→16kHz) → VAD → [channel] → ASR → TranscriptEvent
///
/// The ingestion loop and ASR loop run as separate tasks so that RTP processing
/// is never blocked by ASR inference.
#[allow(dead_code)]
pub struct TranscriptionWorker {
    user_id: ObjectId,
    room_id: ObjectId,
    speaker_name: String,
    asr: Arc<dyn AsrBackend>,
    config: TranscriptionConfig,
    rtp_rx: mpsc::Receiver<Vec<u8>>,
    transcript_tx: broadcast::Sender<TranscriptEvent>,
}

impl TranscriptionWorker {
    pub fn new(
        user_id: ObjectId,
        room_id: ObjectId,
        speaker_name: String,
        asr: Arc<dyn AsrBackend>,
        config: TranscriptionConfig,
        rtp_rx: mpsc::Receiver<Vec<u8>>,
        transcript_tx: broadcast::Sender<TranscriptEvent>,
    ) -> Self {
        Self {
            user_id,
            room_id,
            speaker_name,
            asr,
            config,
            rtp_rx,
            transcript_tx,
        }
    }

    /// Runs the worker pipeline until the RTP channel is closed.
    ///
    /// Spawns an ingestion task (RTP → VAD) that feeds speech segments through a channel
    /// to the ASR loop, so RTP processing is never blocked by ASR inference.
    pub async fn run(self) {
        info!(
            user_id = %self.user_id,
            room_id = %self.room_id,
            speaker = %self.speaker_name,
            backend = %self.asr.name(),
            "Transcription worker started"
        );

        let (segment_tx, segment_rx) = mpsc::channel::<SpeechSegment>(16);

        let config_clone = self.config.clone();
        let rtp_rx = self.rtp_rx;
        let rid = self.room_id;
        let uid = self.user_id;
        let ingestion = tokio::spawn(Self::ingestion_loop(rtp_rx, config_clone, segment_tx, rid, uid));

        // Guard ensures ingestion task is aborted even if this future is cancelled
        // (e.g., by AbortHandle). Dropping a JoinHandle does NOT abort the task,
        // so we must abort explicitly.
        let _ingestion_guard = AbortOnDrop(ingestion);

        Self::asr_loop(
            segment_rx,
            self.asr,
            self.config,
            self.user_id,
            self.room_id,
            self.speaker_name,
            self.transcript_tx,
        )
        .await;

        debug!("Transcription worker stopped");
    }

    /// Ingestion loop: RTP parse → Opus decode → Resample → VAD → SpeechSegment.
    ///
    /// Runs independently so that incoming RTP packets are always processed even
    /// while the ASR loop is busy with inference.
    async fn ingestion_loop(
        mut rtp_rx: mpsc::Receiver<Vec<u8>>,
        config: TranscriptionConfig,
        segment_tx: mpsc::Sender<SpeechSegment>,
        room_id: ObjectId,
        user_id: ObjectId,
    ) {
        let mut opus_decoder = match OpusDecoder::new() {
            Ok(d) => d,
            Err(e) => {
                error!("Failed to create Opus decoder: {}", e);
                return;
            }
        };

        let mut resampler = match Resampler::new(960) {
            Ok(r) => r,
            Err(e) => {
                error!("Failed to create resampler: {}", e);
                return;
            }
        };

        #[cfg(feature = "vad")]
        let mut vad = {
            let vad_path = config
                .vad_model_path
                .as_deref()
                .unwrap_or("models/silero_vad.onnx");
            match SileroVad::new(vad_path, &config) {
                Ok(v) => v,
                Err(e) => {
                    error!("Failed to create VAD: {}", e);
                    return;
                }
            }
        };

        let mut last_seq: Option<u16> = None;
        let mut rtp_count: u64 = 0;
        #[cfg(feature = "vad")]
        let start_time = std::time::Instant::now();
        let mut rtp_timeout_warned = false;

        // Sliding window state for partial results
        #[cfg(feature = "vad")]
        let partial_interval = std::time::Duration::from_millis(
            config.streaming_partial_interval_ms,
        );
        #[cfg(feature = "vad")]
        let mut last_partial_at = std::time::Instant::now();
        #[cfg(feature = "vad")]
        let mut segment_start_time: f64 = 0.0;
        // Minimum accumulated audio samples before emitting a partial (0.5s @ 16kHz)
        #[cfg(feature = "vad")]
        const MIN_PARTIAL_SAMPLES: usize = 8000;

        loop {
            let rtp_data = if !rtp_timeout_warned && rtp_count == 0 {
                // On first iteration, use a 5-second timeout to detect ICE connectivity issues.
                match tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    rtp_rx.recv(),
                ).await {
                    Ok(Some(data)) => data,
                    Ok(None) => break,    // channel closed
                    Err(_) => {
                        warn!(
                            "No RTP packets received within 5 seconds — \
                             WebRTC ICE connectivity may have failed. \
                             Check browser console and chrome://webrtc-internals/"
                        );
                        rtp_timeout_warned = true;
                        match rtp_rx.recv().await {
                            Some(data) => data,
                            None => break,
                        }
                    }
                }
            } else {
                match rtp_rx.recv().await {
                    Some(data) => data,
                    None => break,
                }
            };
            rtp_count += 1;
            if rtp_count == 1 || rtp_count.is_multiple_of(500) {
                info!(rtp_count, bytes = rtp_data.len(), "RTP packets received");
            }

            // 1. Parse RTP
            let rtp_packet = match RtpPacket::parse(&rtp_data) {
                Some(p) => p,
                None => {
                    warn!("Invalid RTP packet, skipping");
                    continue;
                }
            };

            let payload = rtp_packet.payload(&rtp_data);
            if payload.is_empty() {
                continue;
            }

            // Check for packet loss
            if let Some(prev) = last_seq {
                let expected = prev.wrapping_add(1);
                if rtp_packet.header.sequence_number != expected {
                    let gap = rtp_packet
                        .header
                        .sequence_number
                        .wrapping_sub(prev)
                        .wrapping_sub(1);
                    debug!(gap, "RTP packet loss detected, running PLC");
                    for _ in 0..gap.min(3) {
                        if let Ok(pcm) = opus_decoder.decode_plc()
                            && let Ok(resampled) = resampler.process(&pcm)
                        {
                            #[cfg(feature = "vad")]
                            {
                                let _ = vad.process(&resampled);
                            }
                            #[cfg(not(feature = "vad"))]
                            let _ = resampled;
                        }
                    }
                }
            }
            last_seq = Some(rtp_packet.header.sequence_number);

            // 2. Decode Opus → 48kHz mono PCM
            let pcm_48k = match opus_decoder.decode_to_mono(payload) {
                Ok(p) => p,
                Err(e) => {
                    warn!("Opus decode error: {}", e);
                    continue;
                }
            };

            // 3. Resample 48kHz → 16kHz
            let pcm_16k = match resampler.process(&pcm_48k) {
                Ok(p) => p,
                Err(e) => {
                    warn!("Resample error: {}", e);
                    continue;
                }
            };

            if pcm_16k.is_empty() {
                continue;
            }

            // 4. Feed to VAD
            #[cfg(feature = "vad")]
            {
                let conf_hex = room_id.to_hex();
                let user_hex = user_id.to_hex();
                let events = vad.process(&pcm_16k);
                for event in events {
                    match event {
                        VadEvent::SpeechStart => {
                            let elapsed = start_time.elapsed().as_secs_f64();
                            segment_start_time = elapsed;
                            last_partial_at = std::time::Instant::now();
                            debug!("Speech started at {:.3}s", elapsed);
                        }
                        VadEvent::SpeechEnd {
                            audio,
                            duration_secs,
                        } => {
                            let elapsed = start_time.elapsed().as_secs_f64();
                            let seg_start = elapsed - duration_secs;
                            info!(
                                duration_secs,
                                samples = audio.len(),
                                "Speech segment ended, sending FINAL to ASR"
                            );

                            let segment = SpeechSegment {
                                audio,
                                start_time: seg_start,
                                end_time: elapsed,
                                is_final: true,
                                segment_id: format!(
                                    "{}:{}:{:.3}",
                                    conf_hex, user_hex, segment_start_time,
                                ),
                            };

                            if segment_tx.send(segment).await.is_err() {
                                debug!("ASR loop closed, stopping ingestion");
                                return;
                            }
                        }
                    }
                }

                // Emit PARTIAL result if speech is active and enough time/audio has accumulated
                if vad.is_speech_active()
                    && last_partial_at.elapsed() >= partial_interval
                    && vad.speech_buffer().len() >= MIN_PARTIAL_SAMPLES
                {
                    let elapsed = start_time.elapsed().as_secs_f64();
                    let partial_audio = vad.speech_buffer().to_vec();
                    debug!(
                        samples = partial_audio.len(),
                        "Emitting PARTIAL segment for sliding window ASR"
                    );

                    let segment = SpeechSegment {
                        audio: partial_audio,
                        start_time: segment_start_time,
                        end_time: elapsed,
                        is_final: false,
                        segment_id: format!(
                            "{}:{}:{:.3}",
                            conf_hex, user_hex, segment_start_time,
                        ),
                    };

                    if segment_tx.send(segment).await.is_err() {
                        debug!("ASR loop closed, stopping ingestion");
                        return;
                    }
                    last_partial_at = std::time::Instant::now();
                }
            }

            #[cfg(not(feature = "vad"))]
            {
                let _ = pcm_16k;
            }
        }

        debug!("RTP channel closed, ingestion loop exiting");
    }

    /// ASR loop: receives speech segments, runs transcription, emits TranscriptEvents.
    async fn asr_loop(
        mut segment_rx: mpsc::Receiver<SpeechSegment>,
        asr: Arc<dyn AsrBackend>,
        config: TranscriptionConfig,
        user_id: ObjectId,
        room_id: ObjectId,
        speaker_name: String,
        transcript_tx: broadcast::Sender<TranscriptEvent>,
    ) {
        while let Some(segment) = segment_rx.recv().await {
            let request = AsrRequest {
                audio_pcm_16k_mono: segment.audio,
                language_hint: config.language.clone(),
                sample_rate: 16000,
            };

            let start = std::time::Instant::now();
            match asr.transcribe(request).await {
                Ok(result) => {
                    let inference_duration_ms = start.elapsed().as_millis() as u64;
                    let text = result.text.trim().to_string();
                    if text.is_empty() || is_hallucination(&text) {
                        debug!("ASR returned empty/hallucinated text: '{}', skipping", text);
                        continue;
                    }

                    let event = TranscriptEvent {
                        room_id,
                        user_id,
                        speaker_name: speaker_name.clone(),
                        text,
                        language: result.language,
                        confidence: result.confidence,
                        start_time: segment.start_time,
                        end_time: segment.end_time,
                        inference_duration_ms,
                        is_final: segment.is_final,
                        segment_id: segment.segment_id.clone(),
                    };

                    if let Err(e) = transcript_tx.send(event) {
                        debug!("No transcript subscribers: {}", e);
                    }
                }
                Err(e) => {
                    warn!("ASR transcription error: {}", e);
                }
            }
        }
    }
}

/// Returns true if the text is a known Whisper hallucination/placeholder.
fn is_hallucination(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("[blank_audio]")
        || lower.contains("[silence]")
        || lower.contains("[music]")
        || lower.contains("(silence)")
        || lower.contains("(music)")
        || lower == "you"
        || lower == "thank you."
        || lower == "thanks for watching!"
}

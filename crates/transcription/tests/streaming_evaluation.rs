//! Streaming transcription evaluation tests.
//!
//! Tests the PARTIAL -> FINAL pattern with sliding window re-transcription.
//!
//! Run with:
//! ```
//! cargo test -p roomler2-transcription --test streaming_evaluation \
//!   --features "local-onnx,vad" -- --nocapture
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use bson::oid::ObjectId;
use roomler2_transcription::asr::{AsrBackend, AsrRequest};
use roomler2_transcription::config::TranscriptionConfig;
use roomler2_transcription::pipeline::{parse_txt, read_wav_16k_mono};
use roomler2_transcription::vad::{SileroVad, VadEvent};
use roomler2_transcription::wer;

/// VAD chunk size matching SileroVad: 512 samples @ 16kHz = 32ms
const VAD_CHUNK_SIZE: usize = 512;
const SAMPLE_RATE: u32 = 16000;
/// Only process first 60 seconds
const MAX_AUDIO_SECS: f64 = 60.0;
/// Minimum samples before emitting a partial (0.5s @ 16kHz)
const MIN_PARTIAL_SAMPLES: usize = 8000;
/// Interval between partial emissions
const PARTIAL_INTERVAL_MS: u64 = 500;

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn media_dir() -> PathBuf {
    project_root().join("media")
}

/// A streaming segment result (partial or final).
#[derive(Debug, Clone)]
struct StreamingSegment {
    segment_id: String,
    text: String,
    is_final: bool,
    start_time: f64,
    end_time: f64,
}

/// Runs the sliding window streaming pipeline on a WAV file.
///
/// Returns all PARTIAL and FINAL segments emitted during processing.
async fn run_streaming_pipeline(
    backend: Arc<dyn AsrBackend>,
    config: &TranscriptionConfig,
    audio: &[f32],
    language_hint: Option<String>,
) -> Vec<StreamingSegment> {
    let vad_model_path = project_root().join("models").join("silero_vad.onnx");
    let mut vad = SileroVad::new(vad_model_path.to_str().unwrap(), config)
        .expect("Failed to create SileroVad");

    let conf_id = ObjectId::new();
    let user_id = ObjectId::new();
    let conf_hex = conf_id.to_hex();
    let user_hex = user_id.to_hex();

    let mut results: Vec<StreamingSegment> = Vec::new();
    let mut sample_offset = 0usize;
    let mut segment_start_time: f64 = 0.0;
    // Track audio-time of last partial (not wall-clock, since file processing is faster than real-time)
    let mut last_partial_sample_offset: usize = 0;
    let partial_interval_samples = (PARTIAL_INTERVAL_MS as f64 / 1000.0 * SAMPLE_RATE as f64) as usize;

    for chunk in audio.chunks(VAD_CHUNK_SIZE) {
        if chunk.len() < VAD_CHUNK_SIZE {
            break;
        }

        let events = vad.process(chunk);
        for event in events {
            match event {
                VadEvent::SpeechStart => {
                    segment_start_time = sample_offset as f64 / SAMPLE_RATE as f64;
                    last_partial_sample_offset = sample_offset;
                }
                VadEvent::SpeechEnd {
                    audio: speech_audio,
                    duration_secs,
                } => {
                    let end_secs = sample_offset as f64 / SAMPLE_RATE as f64;
                    let start_secs = end_secs - duration_secs;
                    let segment_id = format!("{}:{}:{:.3}", conf_hex, user_hex, segment_start_time);

                    // FINAL transcription
                    let request = AsrRequest {
                        audio_pcm_16k_mono: speech_audio,
                        language_hint: language_hint.clone(),
                        sample_rate: SAMPLE_RATE,
                    };

                    if let Ok(result) = backend.transcribe(request).await {
                        let text = result.text.trim().to_string();
                        if !text.is_empty() {
                            results.push(StreamingSegment {
                                segment_id,
                                text,
                                is_final: true,
                                start_time: start_secs,
                                end_time: end_secs,
                            });
                        }
                    }
                }
            }
        }

        // Emit PARTIAL if speech is active and enough audio-time has accumulated since last partial
        if vad.is_speech_active()
            && (sample_offset - last_partial_sample_offset) >= partial_interval_samples
            && vad.speech_buffer().len() >= MIN_PARTIAL_SAMPLES
        {
            let current_secs = sample_offset as f64 / SAMPLE_RATE as f64;
            let segment_id = format!("{}:{}:{:.3}", conf_hex, user_hex, segment_start_time);
            let partial_audio = vad.speech_buffer().to_vec();

            let request = AsrRequest {
                audio_pcm_16k_mono: partial_audio,
                language_hint: language_hint.clone(),
                sample_rate: SAMPLE_RATE,
            };

            if let Ok(result) = backend.transcribe(request).await {
                let text = result.text.trim().to_string();
                if !text.is_empty() {
                    results.push(StreamingSegment {
                        segment_id,
                        text,
                        is_final: false,
                        start_time: segment_start_time,
                        end_time: current_secs,
                    });
                }
            }
            last_partial_sample_offset = sample_offset;
        }

        sample_offset += chunk.len();
    }

    results
}

#[cfg(feature = "local-onnx")]
#[tokio::test]
async fn streaming_canary_partial_before_final() {
    eprintln!("\n=== Streaming Canary: PARTIAL before FINAL ===\n");

    let model_dir = project_root().join("models").join("canary-1b-v2");
    if !model_dir.exists() {
        eprintln!("  Canary model not found at {}, skipping", model_dir.display());
        return;
    }

    let backend = roomler2_transcription::asr::local_onnx::LocalOnnxBackend::new(
        model_dir.to_str().unwrap(),
    )
    .expect("Failed to load Canary model");

    let config = TranscriptionConfig {
        vad_start_threshold: 0.45,
        vad_end_threshold: 0.30,
        vad_min_silence_frames: 20,
        vad_pre_speech_pad_frames: 15,
        streaming_partial_interval_ms: PARTIAL_INTERVAL_MS,
        ..TranscriptionConfig::default()
    };

    let wav_path = media_dir().join("broadcast_1.wav");
    let (audio, sr) = read_wav_16k_mono(&wav_path).expect("Failed to read WAV");
    let max_samples = (MAX_AUDIO_SECS * sr as f64) as usize;
    let audio = if audio.len() > max_samples {
        audio[..max_samples].to_vec()
    } else {
        audio
    };

    let results = run_streaming_pipeline(
        Arc::new(backend),
        &config,
        &audio,
        Some("de".into()),
    )
    .await;

    eprintln!("  Total streaming segments: {}", results.len());
    let partial_count = results.iter().filter(|r| !r.is_final).count();
    let final_count = results.iter().filter(|r| r.is_final).count();
    eprintln!("  PARTIAL: {}, FINAL: {}", partial_count, final_count);

    for seg in &results {
        eprintln!(
            "    [{}] {:.2}s-{:.2}s sid={} \"{}\"",
            if seg.is_final { "FINAL" } else { "PARTIAL" },
            seg.start_time,
            seg.end_time,
            &seg.segment_id[..20.min(seg.segment_id.len())],
            seg.text,
        );
    }

    // Verify: at least some PARTIALs exist (for utterances > 1s)
    assert!(
        partial_count > 0,
        "Expected at least one PARTIAL segment, got none"
    );

    // Verify: all FINALs exist
    assert!(
        final_count > 0,
        "Expected at least one FINAL segment, got none"
    );

    // Verify: for each FINAL, check that PARTIALs with same segment_id came before it
    let final_ids: Vec<&str> = results
        .iter()
        .filter(|r| r.is_final)
        .map(|r| r.segment_id.as_str())
        .collect();

    for fid in &final_ids {
        let partials_for_id: Vec<&StreamingSegment> = results
            .iter()
            .filter(|r| !r.is_final && r.segment_id == *fid)
            .collect();

        // Not all segments may have partials (short utterances < 0.5s skip)
        if !partials_for_id.is_empty() {
            // Verify partials came before the final in the results list
            let final_idx = results.iter().position(|r| r.is_final && r.segment_id == *fid).unwrap();
            for partial in &partials_for_id {
                let partial_idx = results.iter().position(|r| std::ptr::eq(r, *partial)).unwrap();
                assert!(
                    partial_idx < final_idx,
                    "PARTIAL at idx {} came after FINAL at idx {} for segment_id {}",
                    partial_idx,
                    final_idx,
                    fid,
                );
            }
        }
    }

    // Verify: segment_id is consistent (format: conf_hex:user_hex:start_time)
    for seg in &results {
        let parts: Vec<&str> = seg.segment_id.split(':').collect();
        assert_eq!(
            parts.len(),
            3,
            "segment_id should have 3 parts: {}",
            seg.segment_id,
        );
    }
}

#[cfg(feature = "local-onnx")]
#[tokio::test]
async fn streaming_canary_final_wer() {
    eprintln!("\n=== Streaming Canary: FINAL WER ===\n");

    let model_dir = project_root().join("models").join("canary-1b-v2");
    if !model_dir.exists() {
        eprintln!("  Canary model not found at {}, skipping", model_dir.display());
        return;
    }

    let backend = roomler2_transcription::asr::local_onnx::LocalOnnxBackend::new(
        model_dir.to_str().unwrap(),
    )
    .expect("Failed to load Canary model");

    let config = TranscriptionConfig {
        vad_start_threshold: 0.45,
        vad_end_threshold: 0.30,
        vad_min_silence_frames: 20,
        vad_pre_speech_pad_frames: 15,
        streaming_partial_interval_ms: PARTIAL_INTERVAL_MS,
        ..TranscriptionConfig::default()
    };

    let wav_path = media_dir().join("broadcast_1.wav");
    let txt_path = media_dir().join("broadcast_1.txt");

    let (audio, sr) = read_wav_16k_mono(&wav_path).expect("Failed to read WAV");
    let max_samples = (MAX_AUDIO_SECS * sr as f64) as usize;
    let audio = if audio.len() > max_samples {
        audio[..max_samples].to_vec()
    } else {
        audio
    };

    let all_entries = parse_txt(&txt_path).expect("Failed to parse TXT");
    let ref_entries: Vec<_> = all_entries
        .into_iter()
        .filter(|e| e.start_secs < MAX_AUDIO_SECS)
        .collect();

    let results = run_streaming_pipeline(
        Arc::new(backend),
        &config,
        &audio,
        Some("de".into()),
    )
    .await;

    // Collect only FINAL segments for WER computation
    let final_segments: Vec<(f64, f64, String)> = results
        .iter()
        .filter(|r| r.is_final)
        .map(|r| (r.start_time, r.end_time, r.text.clone()))
        .collect();

    eprintln!("  FINAL segments for WER: {}", final_segments.len());

    // Align and compute WER (reuse alignment logic from wer_evaluation)
    let mut pairs = Vec::new();
    for entry in &ref_entries {
        let mut best_overlap = 0.0f64;
        let mut best_text = String::new();

        for (asr_start, asr_end, asr_text) in &final_segments {
            let overlap_start = entry.start_secs.max(*asr_start);
            let overlap_end = entry.end_secs.min(*asr_end);
            let overlap = (overlap_end - overlap_start).max(0.0);

            if overlap > best_overlap {
                best_overlap = overlap;
                best_text = asr_text.clone();
            }
        }

        if best_overlap < 0.1 {
            for (asr_start, asr_end, asr_text) in &final_segments {
                let asr_mid = (asr_start + asr_end) / 2.0;
                if asr_mid >= entry.start_secs - 1.0 && asr_mid <= entry.end_secs + 1.0 {
                    best_text = asr_text.clone();
                    break;
                }
            }
        }

        pairs.push((entry.text.clone(), best_text));
    }

    let (agg_wer, total_edits, total_ref) = wer::aggregate_wer(&pairs);
    eprintln!(
        "\n  Streaming FINAL WER: {:.2}% ({}/{} edits)\n",
        agg_wer * 100.0,
        total_edits,
        total_ref,
    );

    // Streaming should produce same or similar WER as batch mode
    assert!(
        agg_wer < 0.12,
        "Streaming FINAL WER {:.2}% exceeds 12% target",
        agg_wer * 100.0,
    );
}

/// Test NIM gRPC streaming (requires running NIM container).
/// Run with:
/// ```
/// ROOMLER__TRANSCRIPTION__NIM_ENDPOINT=http://localhost:50051 \
/// cargo test -p roomler2-transcription --test streaming_evaluation \
///   --features "remote-nim,vad" -- --ignored --nocapture streaming_nim
/// ```
#[cfg(feature = "remote-nim")]
#[tokio::test]
#[ignore]
async fn streaming_nim_partial_final() {
    use roomler2_transcription::asr::remote_nim::RemoteNimBackend;
    use roomler2_transcription::asr::StreamingAsrBackend;

    eprintln!("\n=== NIM Streaming: PARTIAL/FINAL ===\n");

    let endpoint = std::env::var("ROOMLER__TRANSCRIPTION__NIM_ENDPOINT")
        .unwrap_or_else(|_| "http://localhost:50051".to_string());
    let model = std::env::var("ROOMLER__TRANSCRIPTION__NIM_MODEL").ok();

    let backend = RemoteNimBackend::new(&endpoint, model.as_deref())
        .expect("Failed to create NIM backend");

    assert!(
        backend.supports_streaming(),
        "NIM backend should support streaming"
    );

    let wav_path = media_dir().join("broadcast_1.wav");
    let (audio, sr) = read_wav_16k_mono(&wav_path).expect("Failed to read WAV");
    let max_samples = (MAX_AUDIO_SECS * sr as f64) as usize;
    let audio = if audio.len() > max_samples {
        audio[..max_samples].to_vec()
    } else {
        audio
    };

    // Use native streaming
    let config = roomler2_transcription::asr::StreamingConfig {
        language_hint: Some("de".to_string()),
        sample_rate: SAMPLE_RATE,
    };

    let (audio_tx, mut result_rx) = backend
        .start_stream(config)
        .await
        .expect("Failed to start NIM stream");

    // Send audio in chunks
    let chunk_size = 16000; // 1 second chunks
    let audio_clone = audio.clone();
    tokio::spawn(async move {
        for chunk in audio_clone.chunks(chunk_size) {
            if audio_tx.send(chunk.to_vec()).await.is_err() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        // Drop sender to signal end
        drop(audio_tx);
    });

    // Collect results
    let mut partial_count = 0;
    let mut final_count = 0;
    while let Some(result) = result_rx.recv().await {
        if result.is_final {
            final_count += 1;
            eprintln!("  [FINAL] \"{}\"", result.text);
        } else {
            partial_count += 1;
            eprintln!("  [PARTIAL] \"{}\"", result.text);
        }
    }

    eprintln!("\n  NIM results: {} PARTIAL, {} FINAL", partial_count, final_count);

    assert!(
        final_count > 0,
        "Expected at least one FINAL from NIM streaming"
    );
}

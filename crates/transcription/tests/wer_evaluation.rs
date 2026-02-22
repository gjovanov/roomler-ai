//! WER evaluation tests for Whisper and Canary backends.
//!
//! Run with:
//! ```
//! cargo test -p roomler2-transcription --test wer_evaluation \
//!   --features "local-whisper,local-onnx,vad" -- --nocapture
//! ```
//!
//! These tests read media/broadcast_1.wav, run it through VAD → ASR,
//! and compare against media/broadcast_1.txt reference.

use std::path::PathBuf;
use std::sync::Arc;

use roomler2_transcription::asr::{AsrBackend, AsrRequest};
use roomler2_transcription::config::TranscriptionConfig;
use roomler2_transcription::pipeline::{parse_txt, read_wav_16k_mono, TxtEntry};
use roomler2_transcription::vad::{SileroVad, VadEvent};
use roomler2_transcription::wer;

/// VAD chunk size matching SileroVad: 512 samples @ 16kHz = 32ms
const VAD_CHUNK_SIZE: usize = 512;
const SAMPLE_RATE: u32 = 16000;
/// Only process first 60 seconds (SRT covers ~55s)
const MAX_AUDIO_SECS: f64 = 60.0;

fn project_root() -> PathBuf {
    // crates/transcription/ -> project root is ../../
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

/// A speech segment with timestamp and audio data.
struct SpeechSegment {
    audio: Vec<f32>,
    start_secs: f64,
    end_secs: f64,
}

/// Feed audio through VAD in 512-sample chunks, collect speech segments with timestamps.
fn run_vad(audio: &[f32], config: &TranscriptionConfig) -> Vec<SpeechSegment> {
    let vad_model_path = project_root().join("models").join("silero_vad.onnx");
    let mut vad = SileroVad::new(
        vad_model_path.to_str().unwrap(),
        config,
    )
    .expect("Failed to create SileroVad");

    let mut segments = Vec::new();
    let mut sample_offset = 0usize;

    for chunk in audio.chunks(VAD_CHUNK_SIZE) {
        if chunk.len() < VAD_CHUNK_SIZE {
            break; // skip incomplete final chunk
        }

        let events = vad.process(chunk);
        for event in events {
            match event {
                VadEvent::SpeechStart => {}
                VadEvent::SpeechEnd {
                    audio: speech_audio,
                    duration_secs,
                } => {
                    let end_secs = sample_offset as f64 / SAMPLE_RATE as f64;
                    let start_secs = end_secs - duration_secs;
                    segments.push(SpeechSegment {
                        audio: speech_audio,
                        start_secs,
                        end_secs,
                    });
                }
            }
        }

        sample_offset += chunk.len();
    }

    segments
}

/// Align ASR segments to TXT reference entries by timestamp overlap.
/// Returns (ref_text, hyp_text) pairs.
fn align_segments(
    asr_segments: &[(f64, f64, String)],
    ref_entries: &[TxtEntry],
) -> Vec<(String, String)> {
    let mut pairs = Vec::new();

    for entry in ref_entries {
        // Find the best-matching ASR segment by overlap
        let mut best_overlap = 0.0f64;
        let mut best_text = String::new();

        for (asr_start, asr_end, asr_text) in asr_segments {
            let overlap_start = entry.start_secs.max(*asr_start);
            let overlap_end = entry.end_secs.min(*asr_end);
            let overlap = (overlap_end - overlap_start).max(0.0);

            if overlap > best_overlap {
                best_overlap = overlap;
                best_text = asr_text.clone();
            }
        }

        // Also try finding ASR segments where the ASR midpoint falls within range
        if best_overlap < 0.1 {
            for (asr_start, asr_end, asr_text) in asr_segments {
                let asr_mid = (asr_start + asr_end) / 2.0;
                if asr_mid >= entry.start_secs - 1.0 && asr_mid <= entry.end_secs + 1.0 {
                    best_text = asr_text.clone();
                    break;
                }
            }
        }

        if !best_text.is_empty() {
            pairs.push((entry.text.clone(), best_text));
        } else {
            // No matching ASR segment — count as 100% error
            pairs.push((entry.text.clone(), String::new()));
        }
    }

    pairs
}

/// Core evaluation logic shared between Whisper and Canary tests.
///
/// `language_hint` is passed to each ASR request (e.g. `Some("de")` for German broadcasts).
async fn evaluate_backend(
    backend: Arc<dyn AsrBackend>,
    config: &TranscriptionConfig,
    language_hint: Option<String>,
) -> f64 {
    let wav_path = media_dir().join("broadcast_1.wav");
    let txt_path = media_dir().join("broadcast_1.txt");

    // 1. Read WAV
    let (audio, sr) = read_wav_16k_mono(&wav_path).expect("Failed to read WAV");
    eprintln!(
        "  Audio: {} samples, {}Hz, {:.1}s",
        audio.len(),
        sr,
        audio.len() as f64 / sr as f64
    );

    // Limit to first 60 seconds
    let max_samples = (MAX_AUDIO_SECS * sr as f64) as usize;
    let audio = if audio.len() > max_samples {
        audio[..max_samples].to_vec()
    } else {
        audio
    };

    // 2. Parse TXT reference
    let ref_entries = parse_txt(&txt_path).expect("Failed to parse TXT");
    // Filter to entries within the audio range
    let ref_entries: Vec<TxtEntry> = ref_entries
        .into_iter()
        .filter(|e| e.start_secs < MAX_AUDIO_SECS)
        .collect();
    eprintln!("  Reference entries (within {}s): {}", MAX_AUDIO_SECS, ref_entries.len());
    for entry in &ref_entries {
        eprintln!(
            "    [{:.2}s - {:.2}s] {} {}",
            entry.start_secs,
            entry.end_secs,
            entry.speaker,
            entry.text
        );
    }

    // 3. Run VAD
    let vad_segments = run_vad(&audio, config);
    eprintln!("  VAD segments: {}", vad_segments.len());
    for (i, seg) in vad_segments.iter().enumerate() {
        eprintln!(
            "    Seg {}: [{:.2}s - {:.2}s] {:.1}s, {} samples",
            i,
            seg.start_secs,
            seg.end_secs,
            seg.end_secs - seg.start_secs,
            seg.audio.len()
        );
    }

    // 4. Transcribe each VAD segment
    let mut asr_segments: Vec<(f64, f64, String)> = Vec::new();
    for (i, seg) in vad_segments.iter().enumerate() {
        let request = AsrRequest {
            audio_pcm_16k_mono: seg.audio.clone(),
            language_hint: language_hint.clone(),
            sample_rate: SAMPLE_RATE,
        };

        let start = std::time::Instant::now();
        match backend.transcribe(request).await {
            Ok(result) => {
                let elapsed = start.elapsed();
                let text = result.text.trim().to_string();
                eprintln!(
                    "    ASR {}: [{:.2}s-{:.2}s] ({:?}) lang={:?} \"{}\"",
                    i, seg.start_secs, seg.end_secs, elapsed, result.language, text
                );
                if !text.is_empty() {
                    asr_segments.push((seg.start_secs, seg.end_secs, text));
                }
            }
            Err(e) => {
                eprintln!("    ASR {} FAILED: {}", i, e);
            }
        }
    }

    // 5. Align and compute WER
    let pairs = align_segments(&asr_segments, &ref_entries);

    eprintln!("\n  === Per-segment WER ===");
    for (i, (ref_text, hyp_text)) in pairs.iter().enumerate() {
        let (seg_wer, edits, ref_count) = wer::word_error_rate(ref_text, hyp_text);
        eprintln!(
            "    Seg {}: WER={:.1}% ({}/{} edits)",
            i,
            seg_wer * 100.0,
            edits,
            ref_count
        );
        eprintln!("      REF: {}", ref_text);
        eprintln!("      HYP: {}", hyp_text);
    }

    let (agg_wer, total_edits, total_ref) = wer::aggregate_wer(&pairs);
    eprintln!(
        "\n  === Aggregate WER: {:.2}% ({}/{} word edits) ===\n",
        agg_wer * 100.0,
        total_edits,
        total_ref
    );

    agg_wer
}

#[tokio::test]
async fn evaluate_whisper_wer() {
    eprintln!("\n=== Whisper WER Evaluation ===\n");

    // Try small multilingual first, fall back to base.en
    let model_dir = project_root().join("models");
    let model_path = if model_dir.join("ggml-small.bin").exists() {
        model_dir.join("ggml-small.bin")
    } else {
        model_dir.join("ggml-base.en.bin")
    };

    eprintln!("  Model: {}", model_path.display());

    let backend = roomler2_transcription::asr::local_whisper::LocalWhisperBackend::new(
        model_path.to_str().unwrap(),
        None,
    )
    .expect("Failed to load Whisper model");

    let config = TranscriptionConfig {
        vad_start_threshold: 0.45,
        vad_end_threshold: 0.30,
        vad_min_silence_frames: 20,
        vad_pre_speech_pad_frames: 15,
        ..TranscriptionConfig::default()
    };

    // Pass "de" hint — the broadcast is German. Without this, Whisper
    // auto-detects the language and may translate to English instead of
    // transcribing in the source language.
    let wer_val = evaluate_backend(Arc::new(backend), &config, Some("de".into())).await;

    eprintln!(
        "  Whisper WER: {:.2}% (target < 20%)",
        wer_val * 100.0
    );

    // Whisper with ggml-small on German broadcast: ~15% WER is expected
    // due to proper nouns (Italian cultural names, Austrian geography).
    // Threshold is generous; tighten as model/config improves.
    if model_dir.join("ggml-small.bin").exists() {
        assert!(
            wer_val < 0.20,
            "Whisper WER {:.2}% exceeds 20% target",
            wer_val * 100.0
        );
    } else {
        eprintln!("  NOTE: Using base.en model — strict WER assertion skipped");
        eprintln!("  Download ggml-small.bin for full evaluation");
    }
}

#[tokio::test]
async fn evaluate_canary_wer() {
    eprintln!("\n=== Canary WER Evaluation ===\n");

    let model_dir = project_root().join("models").join("canary-1b-v2");
    if !model_dir.exists() {
        eprintln!("  Canary model not found at {}, skipping", model_dir.display());
        return;
    }

    eprintln!("  Model: {}", model_dir.display());

    let backend = roomler2_transcription::asr::local_onnx::LocalOnnxBackend::new(
        model_dir.to_str().unwrap(),
    )
    .expect("Failed to load Canary model");

    let config = TranscriptionConfig {
        vad_start_threshold: 0.45,
        vad_end_threshold: 0.30,
        vad_min_silence_frames: 20,
        vad_pre_speech_pad_frames: 15,
        ..TranscriptionConfig::default()
    };

    // Canary has built-in dual-pass language detection (en/de), but
    // passing "de" explicitly gives single-pass with known language.
    let wer_val = evaluate_backend(Arc::new(backend), &config, Some("de".into())).await;

    eprintln!(
        "  Canary WER: {:.2}% (target < 12%)",
        wer_val * 100.0
    );

    // Canary-1B-v2 on German broadcast: ~8% WER expected.
    // Errors are mainly Italian/Austrian proper nouns that the model
    // hasn't seen in training (Società Dante Alighieri, Walserberg, etc.)
    assert!(
        wer_val < 0.12,
        "Canary WER {:.2}% exceeds 12% target",
        wer_val * 100.0
    );
}

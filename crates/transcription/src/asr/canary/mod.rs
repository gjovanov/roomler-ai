//! Canary-1B-v2 ONNX backend for speech recognition.
//!
//! Ported from parakeet-rs. Self-contained module with:
//! - Mel spectrogram extraction (preemphasis, STFT via rustfft, mel filterbank)
//! - SentencePiece tokenizer from vocab.txt
//! - Encoder-decoder ONNX inference with KV-cache greedy decoding
//!
//! Model source: <https://huggingface.co/istupakov/canary-1b-v2-onnx>

use ndarray::{Array1, Array2, Array3, Array4};
use ort::session::Session;
use std::collections::HashMap;
use std::f32::consts::PI;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

// ============================================================================
// Constants
// ============================================================================

const SAMPLE_RATE: usize = 16000;
const N_MELS: usize = 128;
const N_FFT: usize = 512;
const HOP_LENGTH: usize = 160;
const WIN_LENGTH: usize = 400;
const PREEMPHASIS: f32 = 0.97;

// Special token IDs (from vocab.txt)
const ENDOFTEXT_ID: i64 = 3;
const STARTOFTRANSCRIPT_ID: i64 = 4;
const PNC_ID: i64 = 5;
const STARTOFCONTEXT_ID: i64 = 7;
const NOITN_ID: i64 = 9;
const NOTIMESTAMP_ID: i64 = 11;
const NODIARIZE_ID: i64 = 13;
const EMO_UNDEFINED_ID: i64 = 16;

const EN_LANG_ID: i64 = 64;
const PREDICT_LANG_ID: i64 = 22;

// ============================================================================
// Audio Processing
// ============================================================================

fn apply_preemphasis(audio: &[f32], coef: f32) -> Vec<f32> {
    let mut result = Vec::with_capacity(audio.len());
    result.push(audio[0]);
    for i in 1..audio.len() {
        result.push(audio[i] - coef * audio[i - 1]);
    }
    result
}

fn hann_window(window_length: usize) -> Vec<f32> {
    (0..window_length)
        .map(|i| 0.5 - 0.5 * ((2.0 * PI * i as f32) / (window_length as f32 - 1.0)).cos())
        .collect()
}

fn stft(audio: &[f32], n_fft: usize, hop_length: usize, win_length: usize) -> Array2<f32> {
    use rustfft::{num_complex::Complex, FftPlanner};

    let window = hann_window(win_length);
    let num_frames = (audio.len().saturating_sub(win_length)) / hop_length + 1;
    let freq_bins = n_fft / 2 + 1;
    let mut spectrogram = Array2::<f32>::zeros((freq_bins, num_frames));

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(n_fft);

    for frame_idx in 0..num_frames {
        let start = frame_idx * hop_length;
        let mut frame: Vec<Complex<f32>> = vec![Complex::new(0.0, 0.0); n_fft];
        for i in 0..win_length.min(audio.len() - start) {
            frame[i] = Complex::new(audio[start + i] * window[i], 0.0);
        }
        fft.process(&mut frame);
        for k in 0..freq_bins {
            let magnitude = frame[k].norm();
            spectrogram[[k, frame_idx]] = magnitude * magnitude;
        }
    }

    spectrogram
}

fn hz_to_mel(freq: f32) -> f32 {
    2595.0 * (1.0 + freq / 700.0).log10()
}

fn mel_to_hz(mel: f32) -> f32 {
    700.0 * (10.0_f32.powf(mel / 2595.0) - 1.0)
}

fn create_mel_filterbank(n_mels: usize, sample_rate: usize) -> Array2<f32> {
    let freq_bins = N_FFT / 2 + 1;
    let mut filterbank = Array2::<f32>::zeros((n_mels, freq_bins));

    let min_mel = hz_to_mel(0.0);
    let max_mel = hz_to_mel(sample_rate as f32 / 2.0);

    let mel_points: Vec<f32> = (0..=n_mels + 1)
        .map(|i| mel_to_hz(min_mel + (max_mel - min_mel) * i as f32 / (n_mels + 1) as f32))
        .collect();

    let freq_bin_width = sample_rate as f32 / N_FFT as f32;

    for mel_idx in 0..n_mels {
        let left = mel_points[mel_idx];
        let center = mel_points[mel_idx + 1];
        let right = mel_points[mel_idx + 2];

        for freq_idx in 0..freq_bins {
            let freq = freq_idx as f32 * freq_bin_width;
            if freq >= left && freq <= center {
                filterbank[[mel_idx, freq_idx]] = (freq - left) / (center - left);
            } else if freq > center && freq <= right {
                filterbank[[mel_idx, freq_idx]] = (right - freq) / (right - center);
            }
        }
    }

    filterbank
}

fn normalize_features(mut features: Array2<f32>) -> Array2<f32> {
    let num_frames = features.shape()[0];
    let num_features = features.shape()[1];

    for feat_idx in 0..num_features {
        let mut column = features.column_mut(feat_idx);
        let mean: f32 = column.iter().sum::<f32>() / num_frames as f32;
        let variance: f32 =
            column.iter().map(|&x| (x - mean).powi(2)).sum::<f32>() / num_frames as f32;
        let std = variance.sqrt().max(1e-10);
        for val in column.iter_mut() {
            *val = (*val - mean) / std;
        }
    }

    features
}

// ============================================================================
// Tokenizer
// ============================================================================

pub struct CanaryTokenizer {
    id_to_token: Vec<String>,
    lang_to_id: HashMap<String, i64>,
    id_to_lang: HashMap<i64, String>,
}

impl CanaryTokenizer {
    pub fn from_file<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let file = File::open(path.as_ref())?;
        let reader = BufReader::new(file);
        let mut id_to_token = Vec::new();
        let mut lang_to_id = HashMap::new();
        let mut id_to_lang = HashMap::new();

        for line in reader.lines() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let parts: Vec<&str> = line.rsplitn(2, ' ').collect();
            let (token, id) = if parts.len() == 2 {
                let id: i64 = parts[0].parse()?;
                (parts[1].to_string(), id)
            } else {
                (line.to_string(), id_to_token.len() as i64)
            };

            if id as usize >= id_to_token.len() {
                id_to_token.resize(id as usize + 1, String::new());
            }
            id_to_token[id as usize] = token.clone();

            if token.starts_with("<|") && token.ends_with("|>") && token.len() == 6 {
                let lang_code = &token[2..4];
                lang_to_id.insert(lang_code.to_string(), id);
                id_to_lang.insert(id, lang_code.to_string());
            }
        }

        debug!(
            tokens = id_to_token.len(),
            languages = lang_to_id.len(),
            "Canary tokenizer loaded"
        );

        Ok(Self {
            id_to_token,
            lang_to_id,
            id_to_lang,
        })
    }

    pub fn get_language_id(&self, lang: &str) -> i64 {
        self.lang_to_id.get(lang).copied().unwrap_or(EN_LANG_ID)
    }

    pub fn get_language_from_id(&self, id: i64) -> Option<&str> {
        self.id_to_lang.get(&id).map(|s| s.as_str())
    }

    pub fn decode(&self, token_ids: &[i64]) -> String {
        let mut result = String::new();

        for &id in token_ids {
            if id <= STARTOFTRANSCRIPT_ID || id == ENDOFTEXT_ID {
                continue;
            }

            if let Some(token) = self.id_to_token.get(id as usize) {
                if token.starts_with("<|") && token.ends_with("|>") {
                    continue;
                }

                // SentencePiece ▁ = word boundary (3 bytes in UTF-8)
                if let Some(stripped) = token.strip_prefix('▁') {
                    if !result.is_empty() {
                        result.push(' ');
                    }
                    result.push_str(stripped);
                } else {
                    result.push_str(token);
                }
            }
        }

        result.trim().to_string()
    }
}

// ============================================================================
// Model
// ============================================================================

pub struct CanaryModel {
    encoder: Session,
    decoder: Session,
    tokenizer: CanaryTokenizer,
    language: Option<String>,
    mel_filterbank: Array2<f32>,
    repetition_penalty: f32,
    max_sequence_length: usize,
}

impl CanaryModel {
    /// Load model from directory containing encoder, decoder, and vocab files.
    pub fn from_pretrained<P: AsRef<Path>>(model_dir: P) -> anyhow::Result<Self> {
        let model_dir = model_dir.as_ref();

        let encoder_path = Self::find_model(model_dir, &[
            "encoder-model.int8.onnx",
            "encoder-model.onnx",
            "encoder.onnx",
        ])?;
        let decoder_path = Self::find_model(model_dir, &[
            "decoder-model.int8.onnx",
            "decoder-model.onnx",
            "decoder.onnx",
        ])?;
        let vocab_path = model_dir.join("vocab.txt");

        debug!(?encoder_path, ?decoder_path, "Loading Canary ONNX model");

        let tokenizer = CanaryTokenizer::from_file(&vocab_path)?;

        let encoder = Session::builder()?
            .with_intra_threads(2)?
            .with_inter_threads(1)?
            .commit_from_file(&encoder_path)?;

        let decoder = Session::builder()?
            .with_intra_threads(2)?
            .with_inter_threads(1)?
            .commit_from_file(&decoder_path)?;

        let mel_filterbank = create_mel_filterbank(N_MELS, SAMPLE_RATE);

        debug!(
            encoder_inputs = ?encoder.inputs().iter().map(|i| i.name()).collect::<Vec<_>>(),
            decoder_inputs = ?decoder.inputs().iter().map(|i| i.name()).collect::<Vec<_>>(),
            "Canary model loaded"
        );

        Ok(Self {
            encoder,
            decoder,
            tokenizer,
            language: None,
            mel_filterbank,
            repetition_penalty: 1.2,
            max_sequence_length: 1024,
        })
    }

    fn find_model(dir: &Path, candidates: &[&str]) -> anyhow::Result<PathBuf> {
        for candidate in candidates {
            let path = dir.join(candidate);
            if path.exists() {
                return Ok(path);
            }
        }
        anyhow::bail!("No model found in {} (tried: {:?})", dir.display(), candidates)
    }

    /// Extract mel spectrogram features from raw PCM audio.
    fn extract_features(&self, audio: &[f32]) -> anyhow::Result<Array3<f32>> {
        let audio = apply_preemphasis(audio, PREEMPHASIS);
        let spectrogram = stft(&audio, N_FFT, HOP_LENGTH, WIN_LENGTH);
        let mel_spectrogram = self.mel_filterbank.dot(&spectrogram);
        let mel_spectrogram = mel_spectrogram.mapv(|x| (x.max(1e-10)).ln());
        let mel_spectrogram = mel_spectrogram.t().as_standard_layout().into_owned();
        let mel_spectrogram = normalize_features(mel_spectrogram);

        // Add batch dimension: (time, mels) -> (1, time, mels)
        let mel_3d = mel_spectrogram.insert_axis(ndarray::Axis(0));

        Ok(mel_3d)
    }

    /// Run encoder on mel features, returns (embeddings, mask).
    fn run_encoder(&mut self, features: &Array3<f32>) -> anyhow::Result<(Array3<f32>, Array2<i64>)> {
        let time_steps = features.shape()[1];
        let n_mels = features.shape()[2];

        // Transpose to (batch, mels, time) as NeMo expects
        let mut transposed = Array3::<f32>::zeros((1, n_mels, time_steps));
        for t in 0..time_steps {
            for m in 0..n_mels {
                transposed[[0, m, t]] = features[[0, t, m]];
            }
        }

        let length = Array1::from_vec(vec![time_steps as i64]);

        let input_value = ort::value::Value::from_array(transposed)?;
        let length_value = ort::value::Value::from_array(length)?;

        let outputs = self.encoder.run(ort::inputs!(
            "audio_signal" => input_value,
            "length" => length_value
        ))?;

        let embeddings = &outputs["encoder_embeddings"];
        let mask = &outputs["encoder_mask"];

        let (emb_shape, emb_data) = embeddings.try_extract_tensor::<f32>()?;
        let (mask_shape, mask_data) = mask.try_extract_tensor::<i64>()?;

        let emb_dims = emb_shape.as_ref();
        let mask_dims = mask_shape.as_ref();

        let encoder_out = Array3::from_shape_vec(
            (emb_dims[0] as usize, emb_dims[1] as usize, emb_dims[2] as usize),
            emb_data.to_vec(),
        )?;

        let encoder_mask = Array2::from_shape_vec(
            (mask_dims[0] as usize, mask_dims[1] as usize),
            mask_data.to_vec(),
        )?;

        Ok((encoder_out, encoder_mask))
    }

    /// Build the 9-token Canary prompt.
    ///
    /// `language`: `Some("en")` → use explicit language token; `None` → use `<|predict_lang|>` for auto-detection.
    fn build_prompt(&self, language: Option<&str>) -> Vec<i64> {
        let lang_id = match language {
            Some(lang) => self.tokenizer.get_language_id(lang),
            None => PREDICT_LANG_ID,
        };
        vec![
            STARTOFCONTEXT_ID,
            STARTOFTRANSCRIPT_ID,
            EMO_UNDEFINED_ID,
            lang_id,
            lang_id,
            PNC_ID,
            NOITN_ID,
            NOTIMESTAMP_ID,
            NODIARIZE_ID,
        ]
    }

    /// Greedy decode with KV cache, falling back to O(n^2) if needed.
    ///
    /// Returns `(token_ids, total_log_prob)`.
    fn greedy_decode(
        &mut self,
        encoder_embeddings: &Array3<f32>,
        encoder_mask: &Array2<i64>,
        language: Option<&str>,
    ) -> anyhow::Result<(Vec<i64>, f64)> {
        match self.greedy_decode_cached(encoder_embeddings, encoder_mask, language) {
            Ok(result) => Ok(result),
            Err(e) => {
                warn!("KV-cache decode failed ({}), falling back to full decode", e);
                self.greedy_decode_full(encoder_embeddings, encoder_mask, language)
            }
        }
    }

    /// O(n) greedy decoding with KV cache.
    ///
    /// Returns `(token_ids, total_log_prob)`.
    fn greedy_decode_cached(
        &mut self,
        encoder_embeddings: &Array3<f32>,
        encoder_mask: &Array2<i64>,
        language: Option<&str>,
    ) -> anyhow::Result<(Vec<i64>, f64)> {
        let mut tokens = self.build_prompt(language);
        let mut decoder_mems = Array4::<f32>::zeros((10, 1, 0, 1024));
        let mut total_log_prob: f64 = 0.0;

        // Step 0: process all prompt tokens
        let (first_token, first_score) = {
            let input_ids = Array2::from_shape_vec((1, tokens.len()), tokens.clone())?;

            let outputs = self.decoder.run(ort::inputs!(
                "input_ids" => ort::value::Value::from_array(input_ids)?,
                "encoder_embeddings" => ort::value::Value::from_array(encoder_embeddings.clone())?,
                "encoder_mask" => ort::value::Value::from_array(encoder_mask.clone())?,
                "decoder_mems" => ort::value::Value::from_array(decoder_mems)?
            ))?;

            decoder_mems = Self::extract_decoder_hidden_states(&outputs)?;
            Self::extract_next_token(&outputs, &tokens, self.repetition_penalty)?
        };

        if first_token == ENDOFTEXT_ID {
            return Ok((tokens, total_log_prob));
        }
        total_log_prob += first_score as f64;
        tokens.push(first_token);

        // Steps 1..N: process one token at a time
        for _step in 1..self.max_sequence_length {
            let last_token = *tokens.last().unwrap();
            let input_ids = Array2::from_shape_vec((1, 1), vec![last_token])?;

            let outputs = self.decoder.run(ort::inputs!(
                "input_ids" => ort::value::Value::from_array(input_ids)?,
                "encoder_embeddings" => ort::value::Value::from_array(encoder_embeddings.clone())?,
                "encoder_mask" => ort::value::Value::from_array(encoder_mask.clone())?,
                "decoder_mems" => ort::value::Value::from_array(decoder_mems)?
            ))?;

            decoder_mems = Self::extract_decoder_hidden_states(&outputs)?;
            let (next_token, score) =
                Self::extract_next_token(&outputs, &tokens, self.repetition_penalty)?;
            if next_token == ENDOFTEXT_ID {
                break;
            }
            total_log_prob += score as f64;
            tokens.push(next_token);
        }

        Ok((tokens, total_log_prob))
    }

    /// O(n^2) greedy decoding without KV cache (fallback).
    ///
    /// Returns `(token_ids, total_log_prob)`.
    fn greedy_decode_full(
        &mut self,
        encoder_embeddings: &Array3<f32>,
        encoder_mask: &Array2<i64>,
        language: Option<&str>,
    ) -> anyhow::Result<(Vec<i64>, f64)> {
        let mut tokens = self.build_prompt(language);
        let mut total_log_prob: f64 = 0.0;

        for _step in 0..self.max_sequence_length {
            let input_ids = Array2::from_shape_vec((1, tokens.len()), tokens.clone())?;
            let decoder_mems = Array4::<f32>::zeros((10, 1, 0, 1024));

            let outputs = self.decoder.run(ort::inputs!(
                "input_ids" => ort::value::Value::from_array(input_ids)?,
                "encoder_embeddings" => ort::value::Value::from_array(encoder_embeddings.clone())?,
                "encoder_mask" => ort::value::Value::from_array(encoder_mask.clone())?,
                "decoder_mems" => ort::value::Value::from_array(decoder_mems)?
            ))?;

            let (next_token, score) =
                Self::extract_next_token(&outputs, &tokens, self.repetition_penalty)?;
            if next_token == ENDOFTEXT_ID {
                break;
            }
            total_log_prob += score as f64;
            tokens.push(next_token);
        }

        Ok((tokens, total_log_prob))
    }

    /// Extract decoder_hidden_states [10, batch, seq_len, 1024] for KV cache.
    fn extract_decoder_hidden_states(
        outputs: &ort::session::SessionOutputs,
    ) -> anyhow::Result<Array4<f32>> {
        let (shape, data) = outputs["decoder_hidden_states"].try_extract_tensor::<f32>()?;
        let dims = shape.as_ref();
        anyhow::ensure!(
            dims.len() == 4,
            "Expected 4D decoder_hidden_states, got {}D: {:?}",
            dims.len(),
            dims
        );
        Ok(Array4::from_shape_vec(
            (dims[0] as usize, dims[1] as usize, dims[2] as usize, dims[3] as usize),
            data.to_vec(),
        )?)
    }

    /// Extract next token from logits with repetition penalty.
    ///
    /// Returns `(token_id, log_prob)` where log_prob is the log-softmax score of the chosen token.
    fn extract_next_token(
        outputs: &ort::session::SessionOutputs,
        tokens: &[i64],
        repetition_penalty: f32,
    ) -> anyhow::Result<(i64, f32)> {
        let (logits_shape, logits_data) = outputs["logits"].try_extract_tensor::<f32>()?;
        let logits_dims = logits_shape.as_ref();
        let vocab_size = logits_dims[2] as usize;
        let seq_len = logits_dims[1] as usize;

        let last_pos_start = (seq_len - 1) * vocab_size;
        let mut last_logits: Vec<f32> =
            logits_data[last_pos_start..last_pos_start + vocab_size].to_vec();

        if repetition_penalty != 1.0 {
            for &token_id in tokens {
                let idx = token_id as usize;
                if idx < last_logits.len() {
                    if last_logits[idx] > 0.0 {
                        last_logits[idx] /= repetition_penalty;
                    } else {
                        last_logits[idx] *= repetition_penalty;
                    }
                }
            }
        }

        let (best_idx, &best_logit) = last_logits
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or((ENDOFTEXT_ID as usize, &0.0));

        // log-softmax: log_prob = logit - log(sum(exp(logits)))
        let max_logit = best_logit;
        let logsumexp = max_logit
            + last_logits
                .iter()
                .map(|&x| (x - max_logit).exp())
                .sum::<f32>()
                .ln();
        let log_prob = best_logit - logsumexp;

        Ok((best_idx as i64, log_prob))
    }

    /// Languages to try during dual-pass auto-detection.
    const AUTO_DETECT_LANGS: &'static [&'static str] = &["en", "de"];

    /// Transcribe raw PCM audio (16kHz mono f32).
    ///
    /// Returns `(text, detected_language)`. If `language_hint` is `Some`, that language is used
    /// explicitly. If `None`, falls back to `self.language`, and if that is also `None`, runs
    /// dual-pass decoding (one per candidate language) and picks the result with the highest
    /// total log-probability.
    pub fn transcribe(
        &mut self,
        samples: &[f32],
        language_hint: Option<&str>,
    ) -> anyhow::Result<(String, Option<String>)> {
        if samples.is_empty() {
            return Ok((String::new(), None));
        }

        // Resolve effective language upfront (clone to avoid borrow conflict with &mut self)
        let effective_lang: Option<String> = language_hint
            .map(|s| s.to_string())
            .or_else(|| self.language.clone());

        let features = self.extract_features(samples)?;
        let (encoder_embeddings, encoder_mask) = self.run_encoder(&features)?;

        if let Some(lang) = effective_lang {
            // Explicit language — single pass
            let (token_ids, _score) = self.greedy_decode(
                &encoder_embeddings,
                &encoder_mask,
                Some(&lang),
            )?;
            let text = self.tokenizer.decode(&token_ids);
            debug!(lang, "Canary explicit language");
            return Ok((text, Some(lang)));
        }

        // Auto-detect: dual-pass decoding, compare log-probs
        let mut best_text = String::new();
        let mut best_lang = String::new();
        let mut best_score = f64::NEG_INFINITY;

        for &lang in Self::AUTO_DETECT_LANGS {
            let (token_ids, score) = self.greedy_decode(
                &encoder_embeddings,
                &encoder_mask,
                Some(lang),
            )?;

            // Normalize score by number of generated tokens to avoid length bias
            let n_generated = token_ids.len().saturating_sub(9); // 9-token prompt
            let avg_score = if n_generated > 0 {
                score / n_generated as f64
            } else {
                score
            };

            let text = self.tokenizer.decode(&token_ids);
            debug!(
                lang,
                score,
                avg_score,
                n_tokens = n_generated,
                text_len = text.len(),
                "Canary auto-detect pass"
            );

            if avg_score > best_score {
                best_score = avg_score;
                best_text = text;
                best_lang = lang.to_string();
            }
        }

        debug!(lang = %best_lang, score = best_score, "Canary auto-detected language");
        Ok((best_text, Some(best_lang)))
    }
}

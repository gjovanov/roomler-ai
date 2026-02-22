use ndarray::{Array0, Array1, Array2, Array3};
use ort::session::Session;
use ort::value::Tensor;
use tracing::{debug, info, warn};

use crate::config::TranscriptionConfig;
use crate::pipeline::AudioRingBuffer;

use super::VadEvent;

/// VAD chunk size: 512 samples at 16kHz = 32ms per frame.
const CHUNK_SIZE: usize = 512;
const SAMPLE_RATE: i64 = 16000;

/// Silero VAD v4: separate h/c states, hidden size 64
const V4_HIDDEN_SIZE: usize = 64;
/// Silero VAD v5: combined state, hidden size 128
const V5_HIDDEN_SIZE: usize = 128;

#[derive(Debug, PartialEq)]
enum VadState {
    Silence,
    Speech,
}

/// Which Silero VAD model version we detected.
#[derive(Debug, Clone, Copy)]
enum ModelVersion {
    /// v4: inputs (input, sr, h, c), outputs (output, hn, cn)
    V4,
    /// v5: inputs (input, state, sr), outputs (output, stateN)
    V5,
}

/// Silero VAD wrapper using ONNX Runtime.
///
/// Operates on 512-sample chunks (32ms) at 16kHz mono.
/// Auto-detects v4 vs v5 model format.
pub struct SileroVad {
    session: Session,
    version: ModelVersion,
    state: VadState,
    /// v4: LSTM hidden state [2, 1, 64]
    h: Array3<f32>,
    /// v4: LSTM cell state [2, 1, 64]
    c: Array3<f32>,
    /// v5: combined state [2, 1, 128]
    combined_state: Array3<f32>,
    /// Consecutive speech frames counter.
    speech_frames: usize,
    /// Consecutive silence frames counter.
    silence_frames: usize,
    /// Accumulated speech audio (16kHz mono).
    speech_buffer: Vec<f32>,
    /// Pre-speech audio ring buffer.
    pre_speech_buffer: AudioRingBuffer,
    /// Total samples processed since speech start (for max duration check).
    speech_sample_count: usize,
    /// Configuration thresholds.
    start_threshold: f32,
    end_threshold: f32,
    min_speech_frames: usize,
    min_silence_frames: usize,
    max_speech_samples: usize,
    /// Buffer for accumulating samples that don't fill a complete chunk.
    pending_samples: Vec<f32>,
    /// Chunk counter for diagnostic logging.
    chunk_count: u64,
}

impl SileroVad {
    /// Creates a new Silero VAD from an ONNX model file.
    pub fn new(model_path: &str, config: &TranscriptionConfig) -> anyhow::Result<Self> {
        let session = Session::builder()
            .map_err(|e| anyhow::anyhow!("Failed to create ORT session builder: {}", e))?
            .with_intra_threads(1)
            .map_err(|e| anyhow::anyhow!("Failed to set intra threads: {}", e))?
            .commit_from_file(model_path)
            .map_err(|e| anyhow::anyhow!("Failed to load VAD model '{}': {}", model_path, e))?;

        // Detect model version by inspecting input names
        let input_names: Vec<String> = session.inputs().iter().map(|i| i.name().to_string()).collect();
        let version = if input_names.iter().any(|n| n == "state") {
            ModelVersion::V5
        } else {
            ModelVersion::V4
        };

        info!(?version, ?input_names, "Silero VAD model loaded");

        let max_speech_samples = (config.max_speech_duration_secs * SAMPLE_RATE as f64) as usize;

        Ok(Self {
            session,
            version,
            state: VadState::Silence,
            h: Array3::zeros((2, 1, V4_HIDDEN_SIZE)),
            c: Array3::zeros((2, 1, V4_HIDDEN_SIZE)),
            combined_state: Array3::zeros((2, 1, V5_HIDDEN_SIZE)),
            speech_frames: 0,
            silence_frames: 0,
            speech_buffer: Vec::new(),
            pre_speech_buffer: AudioRingBuffer::new(config.vad_pre_speech_pad_frames),
            speech_sample_count: 0,
            start_threshold: config.vad_start_threshold,
            end_threshold: config.vad_end_threshold,
            min_speech_frames: config.vad_min_speech_frames,
            min_silence_frames: config.vad_min_silence_frames,
            max_speech_samples,
            pending_samples: Vec::new(),
            chunk_count: 0,
        })
    }

    /// Feeds 16kHz mono audio and returns any completed speech events.
    pub fn process(&mut self, samples: &[f32]) -> Vec<VadEvent> {
        self.pending_samples.extend_from_slice(samples);
        let mut events = Vec::new();

        while self.pending_samples.len() >= CHUNK_SIZE {
            let chunk: Vec<f32> = self.pending_samples.drain(..CHUNK_SIZE).collect();
            events.extend(self.process_chunk(&chunk));
        }

        events
    }

    /// Returns true if speech is currently in progress.
    pub fn is_speech_active(&self) -> bool {
        self.state == VadState::Speech
    }

    /// Returns a clone of the current speech buffer (accumulated audio since SpeechStart).
    pub fn speech_buffer(&self) -> &[f32] {
        &self.speech_buffer
    }

    /// Processes a single 512-sample chunk through the VAD model.
    fn process_chunk(&mut self, chunk: &[f32]) -> Vec<VadEvent> {
        self.chunk_count += 1;

        let speech_prob = match self.run_inference(chunk) {
            Ok(prob) => prob,
            Err(e) => {
                warn!("VAD inference error: {}", e);
                return vec![];
            }
        };

        // Log periodically so we can see if VAD is working
        if self.chunk_count.is_multiple_of(100) {
            let rms: f32 = (chunk.iter().map(|&x| x * x).sum::<f32>() / chunk.len() as f32).sqrt();
            debug!(
                self.chunk_count,
                speech_prob,
                rms,
                threshold = self.start_threshold,
                "VAD probe"
            );
        }

        match self.state {
            VadState::Silence => {
                if speech_prob >= self.start_threshold {
                    self.speech_frames += 1;
                    if self.speech_frames >= self.min_speech_frames {
                        self.state = VadState::Speech;
                        self.silence_frames = 0;
                        self.speech_sample_count = 0;
                        self.speech_buffer = self.pre_speech_buffer.drain_all();
                        self.speech_buffer.extend_from_slice(chunk);
                        self.speech_sample_count += chunk.len();
                        debug!(speech_prob, "VAD: speech started");
                        return vec![VadEvent::SpeechStart];
                    } else {
                        self.pre_speech_buffer.push(chunk.to_vec());
                    }
                } else {
                    self.speech_frames = 0;
                    self.pre_speech_buffer.push(chunk.to_vec());
                }
                vec![]
            }
            VadState::Speech => {
                self.speech_buffer.extend_from_slice(chunk);
                self.speech_sample_count += chunk.len();

                if self.speech_sample_count >= self.max_speech_samples {
                    debug!("VAD: speech force-ended (max duration)");
                    return vec![self.emit_speech_end()];
                }

                if speech_prob < self.end_threshold {
                    self.silence_frames += 1;
                    if self.silence_frames >= self.min_silence_frames {
                        debug!(speech_prob, "VAD: speech ended");
                        return vec![self.emit_speech_end()];
                    }
                } else {
                    self.silence_frames = 0;
                }
                vec![]
            }
        }
    }

    fn emit_speech_end(&mut self) -> VadEvent {
        let audio = std::mem::take(&mut self.speech_buffer);
        let duration_secs = audio.len() as f64 / SAMPLE_RATE as f64;

        self.state = VadState::Silence;
        self.speech_frames = 0;
        self.silence_frames = 0;
        self.speech_sample_count = 0;
        self.pre_speech_buffer.clear();

        VadEvent::SpeechEnd {
            audio,
            duration_secs,
        }
    }

    /// Runs the Silero VAD ONNX model on a 512-sample chunk.
    fn run_inference(&mut self, chunk: &[f32]) -> anyhow::Result<f32> {
        let input = Array2::from_shape_vec((1, CHUNK_SIZE), chunk.to_vec())
            .map_err(|e| anyhow::anyhow!("Input array shape error: {}", e))?;
        let input_val = Tensor::from_array(input)
            .map_err(|e| anyhow::anyhow!("Input tensor error: {}", e))?;

        match self.version {
            ModelVersion::V5 => self.run_inference_v5(input_val),
            ModelVersion::V4 => self.run_inference_v4(input_val),
        }
    }

    /// Silero VAD v5: inputs (input, state, sr), outputs (output, stateN)
    fn run_inference_v5(&mut self, input_val: Tensor<f32>) -> anyhow::Result<f32> {
        let state_val = Tensor::from_array(self.combined_state.clone())
            .map_err(|e| anyhow::anyhow!("State tensor error: {}", e))?;
        // sr must be a 0-d int64 scalar (matching Python's np.array(16000, dtype='int64'))
        let sr = Array0::from_elem((), SAMPLE_RATE);
        let sr_val = Tensor::from_array(sr)
            .map_err(|e| anyhow::anyhow!("SR tensor error: {}", e))?;

        let outputs = self
            .session
            .run(ort::inputs!("input" => input_val, "state" => state_val, "sr" => sr_val))
            .map_err(|e| anyhow::anyhow!("VAD v5 inference error: {}", e))?;

        // Output 0: speech probability
        let (_shape, output_data) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow::anyhow!("Output extraction error: {}", e))?;
        let speech_prob = output_data.first().copied().unwrap_or(0.0);

        // Output 1: updated state [2, 1, 128]
        let expected = 2 * V5_HIDDEN_SIZE;
        if let Ok((_shape, state_data)) = outputs[1].try_extract_tensor::<f32>()
            && state_data.len() == expected
        {
            self.combined_state =
                Array3::from_shape_vec((2, 1, V5_HIDDEN_SIZE), state_data.to_vec())
                    .unwrap_or_else(|_| Array3::zeros((2, 1, V5_HIDDEN_SIZE)));
        }

        Ok(speech_prob)
    }

    /// Silero VAD v4: inputs (input, sr, h, c), outputs (output, hn, cn)
    fn run_inference_v4(&mut self, input_val: Tensor<f32>) -> anyhow::Result<f32> {
        let sr = Array1::from_vec(vec![SAMPLE_RATE]);
        let sr_val = Tensor::from_array(sr)
            .map_err(|e| anyhow::anyhow!("SR tensor error: {}", e))?;
        let h_val = Tensor::from_array(self.h.clone())
            .map_err(|e| anyhow::anyhow!("H tensor error: {}", e))?;
        let c_val = Tensor::from_array(self.c.clone())
            .map_err(|e| anyhow::anyhow!("C tensor error: {}", e))?;

        let outputs = self
            .session
            .run(ort::inputs![input_val, sr_val, h_val, c_val])
            .map_err(|e| anyhow::anyhow!("VAD v4 inference error: {}", e))?;

        let (_shape, output_data) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow::anyhow!("Output extraction error: {}", e))?;
        let speech_prob = output_data.first().copied().unwrap_or(0.0);

        let expected = 2 * V4_HIDDEN_SIZE;
        if let Ok((_shape, hn_data)) = outputs[1].try_extract_tensor::<f32>()
            && hn_data.len() == expected
        {
            self.h = Array3::from_shape_vec((2, 1, V4_HIDDEN_SIZE), hn_data.to_vec())
                .unwrap_or_else(|_| Array3::zeros((2, 1, V4_HIDDEN_SIZE)));
        }
        if let Ok((_shape, cn_data)) = outputs[2].try_extract_tensor::<f32>()
            && cn_data.len() == expected
        {
            self.c = Array3::from_shape_vec((2, 1, V4_HIDDEN_SIZE), cn_data.to_vec())
                .unwrap_or_else(|_| Array3::zeros((2, 1, V4_HIDDEN_SIZE)));
        }

        Ok(speech_prob)
    }

    /// Resets the VAD state.
    pub fn reset(&mut self) {
        self.state = VadState::Silence;
        self.h = Array3::zeros((2, 1, V4_HIDDEN_SIZE));
        self.c = Array3::zeros((2, 1, V4_HIDDEN_SIZE));
        self.combined_state = Array3::zeros((2, 1, V5_HIDDEN_SIZE));
        self.speech_frames = 0;
        self.silence_frames = 0;
        self.speech_buffer.clear();
        self.pre_speech_buffer.clear();
        self.speech_sample_count = 0;
        self.pending_samples.clear();
        self.chunk_count = 0;
    }
}

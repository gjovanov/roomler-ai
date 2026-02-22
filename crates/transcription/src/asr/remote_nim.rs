use async_trait::async_trait;
use tokio::sync::mpsc;
use tonic::transport::Channel;
use tracing::{debug, info};

use super::{AsrBackend, AsrRequest, StreamingAsrBackend, StreamingConfig, StreamingResult, TranscriptionResult};

/// Generated Riva ASR gRPC client.
pub mod riva_proto {
    tonic::include_proto!("nvidia.riva.asr");
}

use riva_proto::{
    riva_speech_recognition_client::RivaSpeechRecognitionClient,
    AudioEncoding, RecognitionConfig, RecognizeRequest,
    StreamingRecognitionConfig, StreamingRecognizeRequest,
};

/// NVIDIA NIM remote ASR backend via gRPC (Riva ASR API).
///
/// Supports both batch and streaming recognition using Canary or Parakeet models.
pub struct RemoteNimBackend {
    endpoint: String,
    model_name: String,
}

impl RemoteNimBackend {
    pub fn new(endpoint: &str, model_name: Option<&str>) -> anyhow::Result<Self> {
        Ok(Self {
            endpoint: endpoint.to_string(),
            model_name: model_name.unwrap_or("").to_string(),
        })
    }

    async fn connect(&self) -> anyhow::Result<RivaSpeechRecognitionClient<Channel>> {
        let channel = Channel::from_shared(self.endpoint.clone())
            .map_err(|e| anyhow::anyhow!("Invalid NIM endpoint '{}': {}", self.endpoint, e))?
            .connect()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to connect to NIM at '{}': {}", self.endpoint, e))?;
        Ok(RivaSpeechRecognitionClient::new(channel))
    }

    /// Converts f32 PCM samples to 16-bit little-endian bytes.
    fn pcm_f32_to_s16le(samples: &[f32]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for &sample in samples {
            let clamped = sample.clamp(-1.0, 1.0);
            let s16 = (clamped * 32767.0) as i16;
            bytes.extend_from_slice(&s16.to_le_bytes());
        }
        bytes
    }

    fn make_config(&self, language_hint: Option<&str>, sample_rate: u32) -> RecognitionConfig {
        RecognitionConfig {
            encoding: AudioEncoding::Linear16 as i32,
            sample_rate_hertz: sample_rate as i32,
            language_code: language_hint
                .map(|l| match l {
                    "en" => "en-US",
                    "de" => "de-DE",
                    "fr" => "fr-FR",
                    "es" => "es-ES",
                    other => other,
                })
                .unwrap_or("en-US")
                .to_string(),
            max_alternatives: 1,
            model: self.model_name.clone(),
            enable_automatic_punctuation: true,
        }
    }
}

#[async_trait]
impl AsrBackend for RemoteNimBackend {
    async fn transcribe(&self, request: AsrRequest) -> anyhow::Result<TranscriptionResult> {
        let mut client = self.connect().await?;

        let audio_bytes = Self::pcm_f32_to_s16le(&request.audio_pcm_16k_mono);
        let config = self.make_config(request.language_hint.as_deref(), request.sample_rate);

        let req = RecognizeRequest {
            config: Some(config),
            audio: audio_bytes,
        };

        let response = client
            .recognize(req)
            .await
            .map_err(|e| anyhow::anyhow!("NIM Recognize RPC failed: {}", e))?
            .into_inner();

        // Extract best result
        let text = response
            .results
            .first()
            .and_then(|r| r.alternatives.first())
            .map(|a| a.transcript.clone())
            .unwrap_or_default();

        let confidence = response
            .results
            .first()
            .and_then(|r| r.alternatives.first())
            .map(|a| a.confidence as f64);

        Ok(TranscriptionResult {
            text,
            language: None, // Riva doesn't return detected language in batch mode
            confidence,
        })
    }

    fn name(&self) -> &str {
        "remote_nim"
    }

    fn supports_language(&self, lang: &str) -> bool {
        matches!(lang, "en" | "de" | "fr" | "es")
    }

    fn supports_streaming(&self) -> bool {
        true
    }
}

#[async_trait]
impl StreamingAsrBackend for RemoteNimBackend {
    async fn start_stream(
        &self,
        config: StreamingConfig,
    ) -> anyhow::Result<(mpsc::Sender<Vec<f32>>, mpsc::Receiver<StreamingResult>)> {
        let mut client = self.connect().await?;

        let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<f32>>(64);
        let (result_tx, result_rx) = mpsc::channel::<StreamingResult>(64);

        let recognition_config = self.make_config(config.language_hint.as_deref(), config.sample_rate);

        let streaming_config = StreamingRecognitionConfig {
            config: Some(recognition_config),
            interim_results: true,
        };

        // Create the request stream
        let (grpc_tx, grpc_rx) = mpsc::channel::<StreamingRecognizeRequest>(64);

        // Send config as first message
        let config_msg = StreamingRecognizeRequest {
            streaming_request: Some(
                riva_proto::streaming_recognize_request::StreamingRequest::StreamingConfig(
                    streaming_config,
                ),
            ),
        };
        grpc_tx
            .send(config_msg)
            .await
            .map_err(|_| anyhow::anyhow!("Failed to send streaming config"))?;

        // Spawn task to forward audio chunks to gRPC stream
        let grpc_tx_clone = grpc_tx.clone();
        tokio::spawn(async move {
            while let Some(pcm_f32) = audio_rx.recv().await {
                let audio_bytes = Self::pcm_f32_to_s16le(&pcm_f32);
                let msg = StreamingRecognizeRequest {
                    streaming_request: Some(
                        riva_proto::streaming_recognize_request::StreamingRequest::AudioContent(
                            audio_bytes,
                        ),
                    ),
                };
                if grpc_tx_clone.send(msg).await.is_err() {
                    break;
                }
            }
            // Drop grpc_tx_clone to signal end of stream
            drop(grpc_tx_clone);
            debug!("Audio forwarding to gRPC stream complete");
        });

        // Convert mpsc receiver to a tonic stream
        let request_stream = tokio_stream::wrappers::ReceiverStream::new(grpc_rx);

        // Start the streaming RPC
        let mut response_stream = client
            .streaming_recognize(request_stream)
            .await
            .map_err(|e| anyhow::anyhow!("NIM StreamingRecognize RPC failed: {}", e))?
            .into_inner();

        // Spawn task to forward gRPC responses to result channel
        tokio::spawn(async move {
            while let Ok(Some(response)) = response_stream.message().await {
                for result in response.results {
                    if let Some(alt) = result.alternatives.first() {
                        let streaming_result = StreamingResult {
                            text: alt.transcript.clone(),
                            is_final: result.is_final,
                            language: None,
                            confidence: if alt.confidence > 0.0 {
                                Some(alt.confidence as f64)
                            } else {
                                None
                            },
                        };
                        if result_tx.send(streaming_result).await.is_err() {
                            break;
                        }
                    }
                }
            }
            info!("NIM streaming response complete");
        });

        Ok((audio_tx, result_rx))
    }
}

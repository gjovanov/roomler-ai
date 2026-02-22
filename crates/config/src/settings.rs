use config::{Config, ConfigError, Environment, File};
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct Settings {
    pub app: AppSettings,
    pub database: DatabaseSettings,
    pub jwt: JwtSettings,
    pub redis: RedisSettings,
    pub s3: S3Settings,
    pub mediasoup: MediasoupSettings,
    pub turn: TurnSettings,
    pub claude: ClaudeSettings,
    pub oauth: OAuthSettings,
    pub stripe: StripeSettings,
    pub giphy: GiphySettings,
    pub transcription: TranscriptionSettings,
}

#[derive(Debug, Deserialize, Clone)]
pub struct OAuthSettings {
    pub base_url: String,
    pub google: OAuthProviderSettings,
    pub facebook: OAuthProviderSettings,
    pub github: OAuthProviderSettings,
    pub linkedin: OAuthProviderSettings,
    pub microsoft: OAuthProviderSettings,
}

#[derive(Debug, Deserialize, Clone)]
pub struct OAuthProviderSettings {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AppSettings {
    pub host: String,
    pub port: u16,
    pub static_dir: Option<String>,
    pub cors_origins: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DatabaseSettings {
    pub url: String,
    pub name: String,
    pub max_pool_size: Option<u32>,
    pub min_pool_size: Option<u32>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct JwtSettings {
    pub secret: String,
    pub access_token_ttl_secs: u64,
    pub refresh_token_ttl_secs: u64,
    pub issuer: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RedisSettings {
    pub url: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct S3Settings {
    pub endpoint: String,
    pub access_key: String,
    pub secret_key: String,
    pub bucket: String,
    pub region: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MediasoupSettings {
    pub num_workers: u32,
    pub listen_ip: String,
    pub announced_ip: String,
    pub rtc_min_port: u16,
    pub rtc_max_port: u16,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TurnSettings {
    pub url: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub shared_secret: Option<String>,
    pub force_relay: Option<bool>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ClaudeSettings {
    pub api_key: Option<String>,
    pub model: String,
    pub max_tokens: u32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct StripeSettings {
    pub secret_key: String,
    pub publishable_key: String,
    pub webhook_secret: String,
    pub price_pro: String,
    pub price_business: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GiphySettings {
    pub api_key: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TranscriptionSettings {
    pub enabled: bool,
    pub backend: String,
    pub whisper_model_path: Option<String>,
    pub language: Option<String>,
    pub vad_model_path: Option<String>,
    pub vad_start_threshold: f32,
    pub vad_end_threshold: f32,
    pub vad_min_speech_frames: usize,
    pub vad_min_silence_frames: usize,
    pub vad_pre_speech_pad_frames: usize,
    pub max_speech_duration_secs: f64,
    pub nim_endpoint: Option<String>,
    pub onnx_model_path: Option<String>,
    pub nim_model: Option<String>,
    pub streaming_partial_interval_ms: u64,
}

impl Settings {
    pub fn load() -> Result<Self, ConfigError> {
        let config = Config::builder()
            .add_source(File::with_name("config/default").required(false))
            .add_source(File::with_name("config/local").required(false))
            .add_source(
                Environment::default()
                    .separator("__")
                    .prefix("ROOMLER"),
            )
            .set_default("app.host", "0.0.0.0")?
            .set_default("app.port", 3000)?
            .set_default("app.cors_origins", Vec::<String>::new())?
            .set_default("database.url", "mongodb://localhost:27019")?
            .set_default("database.name", "roomler2")?
            .set_default("jwt.secret", "change-me-in-production")?
            .set_default("jwt.access_token_ttl_secs", 3600)?
            .set_default("jwt.refresh_token_ttl_secs", 604800)?
            .set_default("jwt.issuer", "roomler2")?
            .set_default("redis.url", "redis://127.0.0.1:6379")?
            .set_default("s3.endpoint", "http://localhost:9000")?
            .set_default("s3.access_key", "minioadmin")?
            .set_default("s3.secret_key", "minioadmin")?
            .set_default("s3.bucket", "roomler2")?
            .set_default("s3.region", "us-east-1")?
            .set_default("mediasoup.num_workers", 2)?
            .set_default("mediasoup.listen_ip", "0.0.0.0")?
            .set_default("mediasoup.announced_ip", "127.0.0.1")?
            .set_default("mediasoup.rtc_min_port", 40000)?
            .set_default("mediasoup.rtc_max_port", 49999)?
            .set_default("turn.url", None::<String>)?
            .set_default("turn.username", None::<String>)?
            .set_default("turn.password", None::<String>)?
            .set_default("turn.force_relay", false)?
            .set_default("claude.model", "claude-sonnet-4-5-20250929")?
            .set_default("claude.max_tokens", 4096)?
            .set_default("oauth.base_url", "http://localhost:5001")?
            .set_default("oauth.google.client_id", "")?
            .set_default("oauth.google.client_secret", "")?
            .set_default("oauth.facebook.client_id", "")?
            .set_default("oauth.facebook.client_secret", "")?
            .set_default("oauth.github.client_id", "")?
            .set_default("oauth.github.client_secret", "")?
            .set_default("oauth.linkedin.client_id", "")?
            .set_default("oauth.linkedin.client_secret", "")?
            .set_default("oauth.microsoft.client_id", "")?
            .set_default("oauth.microsoft.client_secret", "")?
            .set_default("stripe.secret_key", "")?
            .set_default("stripe.publishable_key", "")?
            .set_default("stripe.webhook_secret", "")?
            .set_default("stripe.price_pro", "")?
            .set_default("stripe.price_business", "")?
            .set_default("giphy.api_key", "")?
            .set_default("transcription.enabled", false)?
            .set_default("transcription.backend", "local_onnx")?
            .set_default("transcription.whisper_model_path", "models/ggml-small.bin")?
            .set_default("transcription.onnx_model_path", "models/canary-1b-v2")?
            .set_default("transcription.vad_model_path", "models/silero_vad.onnx")?
            .set_default("transcription.vad_start_threshold", 0.5)?
            .set_default("transcription.vad_end_threshold", 0.35)?
            .set_default("transcription.vad_min_speech_frames", 3)?
            .set_default("transcription.vad_min_silence_frames", 15)?
            .set_default("transcription.vad_pre_speech_pad_frames", 10)?
            .set_default("transcription.max_speech_duration_secs", 30.0)?
            .set_default("transcription.streaming_partial_interval_ms", 500)?
            .build()?;

        config.try_deserialize()
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self::load().expect("Failed to load default settings")
    }
}

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
    pub email: EmailSettings,
    pub push: PushSettings,
    pub auth: AuthSettings,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct AuthSettings {
    /// When true, `register` sets `is_verified: true` on the new user
    /// directly, bypassing the email activation flow. Default false —
    /// production keeps the email-link activation as the only path.
    /// Used only by the e2e overlay (`ROOMLER__AUTH__AUTO_VERIFY=true`)
    /// so Playwright specs can `register → login` without an SMTP
    /// capture. Phase 4 of the e2e plan replaces this with Mailpit
    /// + a helper that pulls the activation token from the inbox.
    #[serde(default)]
    pub auto_verify: bool,
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
    pub frontend_url: String,
    /// Per-IP rate limit refill rate (tokens/sec). Default 1 — i.e.
    /// sustained 60 req/min. The e2e overlay bumps this so a
    /// Playwright Job's single pod IP doesn't trip 429s during
    /// the suite.
    #[serde(default = "default_rate_limit_per_sec")]
    pub rate_limit_per_sec: u64,
    /// Per-IP rate limit burst cap. Default 60 — sustains the
    /// per-second refill above. Bumped in e2e for the same reason.
    #[serde(default = "default_rate_limit_burst")]
    pub rate_limit_burst: u32,
}

fn default_rate_limit_per_sec() -> u64 {
    1
}
fn default_rate_limit_burst() -> u32 {
    60
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
pub struct EmailSettings {
    pub api_key: String,
    pub from_email: String,
    pub from_name: String,
    pub activation_token_ttl_minutes: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PushSettings {
    pub vapid_public_key: String,
    pub vapid_private_key: String,
    pub contact: String,
}

impl Settings {
    pub fn load() -> Result<Self, ConfigError> {
        let config = Config::builder()
            .add_source(File::with_name("config/default").required(false))
            .add_source(File::with_name("config/local").required(false))
            .add_source(Environment::default().separator("__").prefix("ROOMLER"))
            .set_default("app.host", "0.0.0.0")?
            .set_default("app.port", 3000)?
            .set_default("app.cors_origins", Vec::<String>::new())?
            .set_default("app.frontend_url", "http://localhost:5173")?
            .set_default("app.rate_limit_per_sec", 1)?
            .set_default("app.rate_limit_burst", 60)?
            .set_default("database.url", "mongodb://localhost:27019")?
            .set_default("database.name", "roomler-ai")?
            .set_default("jwt.secret", "change-me-in-production")?
            // Access token: 1 week (was 1 hour). The shorter TTL was
            // partnered with a much longer refresh, but operators
            // were getting kicked back to /login mid-session every
            // hour which the auto-refresh flow doesn't always
            // recover from cleanly. 1-week access trades off some
            // exposure on a leaked token for substantially better
            // session continuity. Refresh extended to 30 days so
            // the refresh > access invariant still holds.
            .set_default("jwt.access_token_ttl_secs", 604800)?
            .set_default("jwt.refresh_token_ttl_secs", 2_592_000)?
            .set_default("jwt.issuer", "roomler-ai")?
            .set_default("redis.url", "redis://127.0.0.1:6379")?
            .set_default("s3.endpoint", "http://localhost:9000")?
            .set_default("s3.access_key", "minioadmin")?
            .set_default("s3.secret_key", "minioadmin")?
            .set_default("s3.bucket", "roomler-ai")?
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
            .set_default("email.api_key", "")?
            .set_default("email.from_email", "noreply@roomler.ai")?
            .set_default("email.from_name", "Roomler")?
            .set_default("email.activation_token_ttl_minutes", 5u64)?
            .set_default("push.vapid_public_key", "")?
            .set_default("push.vapid_private_key", "")?
            .set_default("push.contact", "mailto:noreply@roomler.ai")?
            .set_default("auth.auto_verify", false)?
            .build()?;

        config.try_deserialize()
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self::load().expect("Failed to load default settings")
    }
}

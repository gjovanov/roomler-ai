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
    pub rc: RcSettings,
}

/// Remote-control WS liveness (Phase A-1). Settings (not consts) so the
/// two-pod integration tests can run 2-3 s deadlines instead of 90 s
/// waits. Env: `ROOMLER__RC__WS_RX_DEADLINE_SECS` etc.
#[derive(Debug, Clone, Deserialize)]
pub struct RcSettings {
    /// Reap an agent/tunnel WS after this many seconds without ANY
    /// inbound frame (the agent pings every 25 s and heartbeats every
    /// 30 s, so a healthy leg is never silent this long).
    pub ws_rx_deadline_secs: u64,
    /// How often the read loop checks the deadline.
    pub ws_liveness_tick_secs: u64,
}

impl Default for RcSettings {
    fn default() -> Self {
        Self {
            ws_rx_deadline_secs: 90,
            ws_liveness_tick_secs: 30,
        }
    }
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
    /// Deployment environment label (`"development"` default). Setting it to
    /// `"production"` arms hard-fail guards — currently: refusing to boot
    /// while `jwt.secret` is still the built-in default. Prod sets
    /// `ROOMLER__APP__ENVIRONMENT=production` in the configmap.
    #[serde(default = "default_environment")]
    pub environment: String,
    /// Sustained per-client-IP rate for `/api`, in **requests per second**.
    /// Default 1. The e2e overlay bumps this so a Playwright Job's single
    /// pod IP doesn't trip 429s during the suite.
    ///
    /// Note this is a *rate*, not an interval — see [`Self::rate_limit_period_ms`]
    /// for why that distinction is a trap worth spelling out.
    #[serde(default = "default_rate_limit_per_sec")]
    pub rate_limit_per_sec: u64,
    /// Per-IP burst cap for `/api`. Default 60. This is the real headroom
    /// for a page load: a SPA fans out to many endpoints at once, and every
    /// client behind one NAT shares the bucket.
    #[serde(default = "default_rate_limit_burst")]
    pub rate_limit_burst: u32,
    /// How many reverse proxies *we* control append to `X-Forwarded-For`
    /// before a request reaches axum. Everything further left in that header
    /// is client-supplied and must not be trusted. Prod chain is
    /// docker-nginx (sets the client IP) → pod-nginx (appends its peer), so
    /// exactly 1 hop is ours.
    #[serde(default = "default_rate_limit_trusted_hops")]
    pub rate_limit_trusted_hops: usize,
    /// Sustained auth-endpoint attempts per minute, per (client IP, account).
    /// Deliberately far stricter than the general `/api` limit: this is the
    /// brute-force gate, and it must not depend on starving the whole API.
    #[serde(default = "default_auth_rate_limit_per_min")]
    pub auth_rate_limit_per_min: u32,
    /// Burst cap for the auth limiter, per (client IP, account).
    #[serde(default = "default_auth_rate_limit_burst")]
    pub auth_rate_limit_burst: u32,
}

impl AppSettings {
    /// Replenish interval, in milliseconds, for one `/api` token.
    ///
    /// `tower_governor`'s builder takes the interval that replenishes ONE
    /// cell (`per_second(n)` is `Duration::from_secs(n)`), **not** a rate —
    /// so passing a req/s number straight into `per_second` silently makes
    /// the limit *n* times stricter. Prod hit exactly that on 2026-07-28.
    /// Convert here instead, and clamp: the builder rejects a zero period.
    pub fn rate_limit_period_ms(&self) -> u64 {
        (1000 / self.rate_limit_per_sec.max(1)).max(1)
    }

    /// Replenish interval, in milliseconds, for one auth-endpoint token.
    pub fn auth_rate_limit_period_ms(&self) -> u64 {
        (60_000 / u64::from(self.auth_rate_limit_per_min.max(1))).max(1)
    }
}

fn default_environment() -> String {
    "development".to_string()
}
fn default_rate_limit_per_sec() -> u64 {
    1
}
fn default_rate_limit_burst() -> u32 {
    60
}
fn default_rate_limit_trusted_hops() -> usize {
    1
}
fn default_auth_rate_limit_per_min() -> u32 {
    10
}
fn default_auth_rate_limit_burst() -> u32 {
    20
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
    /// Opt-in switch for the S3/MinIO file backend. The other `s3.*`
    /// fields have always had localhost-MinIO defaults, so their mere
    /// presence can't signal intent — without this flag uploads stay on
    /// the local-disk backend (pod-ephemeral in k8s). Prod sets
    /// `ROOMLER__S3__ENABLED=true`.
    #[serde(default)]
    pub enabled: bool,
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
    /// S6 — per-node announced-IP override for multi-pod hostNetwork
    /// deployments: `"<node_ip>=<public_ip>,<node_ip>=<public_ip>"`.
    /// One Deployment can't template a per-node env value, so the pod
    /// gets its own node IP via the Downward API (`ROOMLER__POD_HOST_IP`,
    /// status.hostIP) and picks its public announced IP from this map.
    /// Absent / no match → `announced_ip` as before.
    #[serde(default)]
    pub announced_ip_map: Option<String>,
    pub rtc_min_port: u16,
    pub rtc_max_port: u16,
}

impl MediasoupSettings {
    /// Resolve the effective announced IP for this pod: exact
    /// `announced_ip_map` match on `pod_host_ip` wins, else the static
    /// `announced_ip` (empty → None, mediasoup announces the listen IP).
    pub fn resolve_announced_ip(&self, pod_host_ip: Option<&str>) -> Option<String> {
        if let (Some(map), Some(host_ip)) = (&self.announced_ip_map, pod_host_ip) {
            for pair in map.split(',') {
                if let Some((node, public)) = pair.split_once('=')
                    && node.trim() == host_ip.trim()
                    && !public.trim().is_empty()
                {
                    return Some(public.trim().to_string());
                }
            }
        }
        if self.announced_ip.is_empty() {
            None
        } else {
            Some(self.announced_ip.clone())
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct TurnSettings {
    pub url: Option<String>,
    /// Same-worker TURN affinity: comma-separated per-worker base URLs, one
    /// per coturn worker (e.g.
    /// "turn:coturn-1.roomler.ai:3478,turn:coturn-2.roomler.ai:3478").
    /// Each is expanded into transport variants like `url`. Unset = no
    /// affinity (single-hostname DNS round-robin behaviour).
    pub worker_urls: Option<String>,
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
    /// SMTP backend host. When set (and `api_key` is empty), the
    /// EmailService dispatches outbound mail over SMTP instead of
    /// SendGrid HTTP. Used by the e2e overlay to capture mail in
    /// Mailpit; not currently used by production.
    #[serde(default)]
    pub smtp_host: Option<String>,
    /// SMTP backend port. Pairs with `smtp_host`. No default — when
    /// `smtp_host` is set you must also set the port (Mailpit
    /// listens on 1025).
    #[serde(default)]
    pub smtp_port: Option<u16>,
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
            .set_default("app.environment", "development")?
            .set_default("app.frontend_url", "http://localhost:5173")?
            .set_default("app.rate_limit_per_sec", 1)?
            .set_default("app.rate_limit_burst", 60)?
            .set_default("app.rate_limit_trusted_hops", 1)?
            .set_default("app.auth_rate_limit_per_min", 10)?
            .set_default("app.auth_rate_limit_burst", 20)?
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
            .set_default("rc.ws_rx_deadline_secs", 90)?
            .set_default("rc.ws_liveness_tick_secs", 30)?
            .set_default("s3.enabled", false)?
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
            .set_default("turn.worker_urls", None::<String>)?
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
            .set_default("stripe.webhook_secret", "")?
            .set_default("stripe.price_pro", "")?
            .set_default("stripe.price_business", "")?
            .set_default("giphy.api_key", "")?
            .set_default("email.api_key", "")?
            .set_default("email.from_email", "noreply@roomler.ai")?
            .set_default("email.from_name", "Roomler")?
            .set_default("email.activation_token_ttl_minutes", 5u64)?
            .set_default("email.smtp_host", None::<String>)?
            .set_default("email.smtp_port", None::<u16>)?
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

#[cfg(test)]
mod rate_limit_tests {
    use super::AppSettings;

    fn app(per_sec: u64, auth_per_min: u32) -> AppSettings {
        AppSettings {
            host: "0.0.0.0".into(),
            port: 3000,
            static_dir: None,
            cors_origins: vec![],
            frontend_url: "http://localhost".into(),
            environment: "development".into(),
            rate_limit_per_sec: per_sec,
            rate_limit_burst: 60,
            rate_limit_trusted_hops: 1,
            auth_rate_limit_per_min: auth_per_min,
            auth_rate_limit_burst: 20,
        }
    }

    /// The whole point of the helper: a req/s number must become a
    /// *shorter* interval, never a longer one. Passing 10 straight into
    /// `per_second` took prod from 1 req/s down to 0.1 req/s on 2026-07-28.
    #[test]
    fn higher_rate_means_shorter_period() {
        assert_eq!(app(1, 10).rate_limit_period_ms(), 1000);
        assert_eq!(app(10, 10).rate_limit_period_ms(), 100);
        assert_eq!(app(100, 10).rate_limit_period_ms(), 10);
        assert!(app(50, 10).rate_limit_period_ms() < app(5, 10).rate_limit_period_ms());
    }

    /// The builder rejects a zero period, so both ends must clamp.
    #[test]
    fn period_never_zero() {
        assert_eq!(app(0, 0).rate_limit_period_ms(), 1000);
        assert_eq!(app(5000, 0).rate_limit_period_ms(), 1);
        assert_eq!(app(1, 0).auth_rate_limit_period_ms(), 60_000);
        assert_eq!(app(1, 120_000).auth_rate_limit_period_ms(), 1);
    }

    #[test]
    fn auth_period_matches_per_minute() {
        assert_eq!(app(1, 10).auth_rate_limit_period_ms(), 6_000);
        assert_eq!(app(1, 60).auth_rate_limit_period_ms(), 1_000);
    }
}

#[cfg(test)]
mod s6_tests {
    use super::MediasoupSettings;

    fn ms(announced: &str, map: Option<&str>) -> MediasoupSettings {
        MediasoupSettings {
            num_workers: 1,
            listen_ip: "0.0.0.0".into(),
            announced_ip: announced.into(),
            announced_ip_map: map.map(|s| s.to_string()),
            rtc_min_port: 40000,
            rtc_max_port: 49999,
        }
    }

    #[test]
    fn announced_ip_map_matches_pod_host() {
        let s = ms(
            "5.9.157.221",
            Some("10.10.30.11=5.9.157.221,10.10.20.11=5.9.157.226"),
        );
        assert_eq!(
            s.resolve_announced_ip(Some("10.10.20.11")).as_deref(),
            Some("5.9.157.226")
        );
        assert_eq!(
            s.resolve_announced_ip(Some("10.10.30.11")).as_deref(),
            Some("5.9.157.221")
        );
    }

    #[test]
    fn announced_ip_map_falls_back_to_static() {
        let s = ms("5.9.157.221", Some("10.10.30.11=5.9.157.221"));
        // Unknown node → static value.
        assert_eq!(
            s.resolve_announced_ip(Some("192.168.1.1")).as_deref(),
            Some("5.9.157.221")
        );
        // No pod host ip (non-k8s run) → static value.
        assert_eq!(s.resolve_announced_ip(None).as_deref(), Some("5.9.157.221"));
        // No map at all → static value.
        let s2 = ms("1.2.3.4", None);
        assert_eq!(
            s2.resolve_announced_ip(Some("10.10.30.11")).as_deref(),
            Some("1.2.3.4")
        );
        // Empty static + no match → None (mediasoup announces listen ip).
        let s3 = ms("", None);
        assert_eq!(s3.resolve_announced_ip(Some("10.10.30.11")), None);
    }

    #[test]
    fn announced_ip_map_tolerates_whitespace_and_junk() {
        let s = ms("", Some(" 10.10.30.11 = 5.9.157.221 ,junk,also=,x"));
        assert_eq!(
            s.resolve_announced_ip(Some("10.10.30.11")).as_deref(),
            Some("5.9.157.221")
        );
        assert_eq!(s.resolve_announced_ip(Some("also")), None);
    }
}

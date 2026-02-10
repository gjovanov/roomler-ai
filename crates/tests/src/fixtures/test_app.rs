use mongodb::{Client, Database, options::ClientOptions};
use roomler2_api::{build_router, state::AppState};
use roomler2_config::Settings;
use roomler2_db::indexes::ensure_indexes;
use std::net::SocketAddr;
use tokio::net::TcpListener;

/// A running test application with its own MongoDB database.
pub struct TestApp {
    pub addr: SocketAddr,
    pub base_url: String,
    pub db: Database,
    pub settings: Settings,
    pub client: reqwest::Client,
}

impl TestApp {
    /// Spawn a new test server connected to the test MongoDB.
    ///
    /// Requires a running MongoDB at localhost:27019.
    /// Set ROOMLER__DATABASE__URL env var to override the connection string.
    /// Each test gets a unique database name for isolation.
    pub async fn spawn() -> Self {
        let db_name = format!("roomler2_test_{}", uuid::Uuid::new_v4().simple());

        let mut settings = Settings::load().unwrap_or_else(|_| {
            // Fallback to minimal settings for tests
            test_settings()
        });
        // Allow env var override for database URL
        if let Ok(url) = std::env::var("ROOMLER__DATABASE__URL") {
            settings.database.url = url;
        }
        settings.database.name = db_name.clone();

        let client_options = ClientOptions::parse(&settings.database.url)
            .await
            .expect("Failed to parse MongoDB URL");
        let mongo_client =
            Client::with_options(client_options).expect("Failed to create MongoDB client");
        let db = mongo_client.database(&db_name);

        ensure_indexes(&db).await.expect("Failed to create indexes");

        let app_state = AppState::new(db.clone(), settings.clone())
            .await
            .expect("Failed to create AppState");
        let app = build_router(app_state);

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Failed to bind to random port");
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let base_url = format!("http://{}", addr);
        let client = reqwest::Client::builder()
            .cookie_store(true)
            .build()
            .expect("Failed to build HTTP client");

        Self {
            addr,
            base_url,
            db,
            settings,
            client,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Spawn a test server with OAuth providers configured (fake client IDs).
    /// Uses a no-redirect reqwest client so we can inspect the 302/307 Location header.
    pub async fn spawn_with_oauth() -> Self {
        let db_name = format!("roomler2_test_{}", uuid::Uuid::new_v4().simple());

        let mut settings = Settings::load().unwrap_or_else(|_| test_settings());
        if let Ok(url) = std::env::var("ROOMLER__DATABASE__URL") {
            settings.database.url = url;
        }
        settings.database.name = db_name.clone();

        // Configure fake OAuth provider credentials
        settings.oauth.base_url = "http://localhost:5001".to_string();
        settings.oauth.google.client_id = "test-google-id".to_string();
        settings.oauth.google.client_secret = "test-google-secret".to_string();
        settings.oauth.facebook.client_id = "test-facebook-id".to_string();
        settings.oauth.facebook.client_secret = "test-facebook-secret".to_string();
        settings.oauth.github.client_id = "test-github-id".to_string();
        settings.oauth.github.client_secret = "test-github-secret".to_string();
        settings.oauth.linkedin.client_id = "test-linkedin-id".to_string();
        settings.oauth.linkedin.client_secret = "test-linkedin-secret".to_string();
        settings.oauth.microsoft.client_id = "test-microsoft-id".to_string();
        settings.oauth.microsoft.client_secret = "test-microsoft-secret".to_string();

        let client_options = ClientOptions::parse(&settings.database.url)
            .await
            .expect("Failed to parse MongoDB URL");
        let mongo_client =
            Client::with_options(client_options).expect("Failed to create MongoDB client");
        let db = mongo_client.database(&db_name);

        ensure_indexes(&db).await.expect("Failed to create indexes");

        let app_state = AppState::new(db.clone(), settings.clone())
            .await
            .expect("Failed to create AppState");
        let app = build_router(app_state);

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Failed to bind to random port");
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let base_url = format!("http://{}", addr);
        // No-redirect client for OAuth redirect tests
        let client = reqwest::Client::builder()
            .cookie_store(true)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("Failed to build HTTP client");

        Self {
            addr,
            base_url,
            db,
            settings,
            client,
        }
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let db = self.db.clone();
        // Best effort cleanup: drop the test database
        tokio::spawn(async move {
            let _ = db.drop().await;
        });
    }
}

fn test_settings() -> Settings {
    Settings {
        app: roomler2_config::AppSettings {
            host: "127.0.0.1".to_string(),
            port: 0,
            static_dir: None,
            cors_origins: vec![],
        },
        database: roomler2_config::DatabaseSettings {
            url: "mongodb://localhost:27019".to_string(),
            name: "roomler2_test".to_string(),
            max_pool_size: Some(5),
            min_pool_size: Some(1),
        },
        jwt: roomler2_config::JwtSettings {
            secret: "test-secret-key-for-jwt-signing-minimum-32-chars".to_string(),
            access_token_ttl_secs: 3600,
            refresh_token_ttl_secs: 604800,
            issuer: "roomler2".to_string(),
        },
        redis: roomler2_config::RedisSettings {
            url: "redis://127.0.0.1:6379".to_string(),
        },
        s3: roomler2_config::S3Settings {
            endpoint: "http://localhost:9000".to_string(),
            access_key: "minioadmin".to_string(),
            secret_key: "minioadmin".to_string(),
            bucket: "roomler2-test".to_string(),
            region: "us-east-1".to_string(),
        },
        mediasoup: roomler2_config::MediasoupSettings {
            num_workers: 1,
            listen_ip: "0.0.0.0".to_string(),
            announced_ip: "127.0.0.1".to_string(),
            rtc_min_port: 40000,
            rtc_max_port: 40100,
        },
        turn: roomler2_config::TurnSettings {
            url: None,
            username: None,
            password: None,
        },
        claude: roomler2_config::ClaudeSettings {
            api_key: None,
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 4096,
        },
        oauth: roomler2_config::OAuthSettings {
            base_url: "http://localhost:5001".to_string(),
            google: roomler2_config::OAuthProviderSettings {
                client_id: String::new(),
                client_secret: String::new(),
            },
            facebook: roomler2_config::OAuthProviderSettings {
                client_id: String::new(),
                client_secret: String::new(),
            },
            github: roomler2_config::OAuthProviderSettings {
                client_id: String::new(),
                client_secret: String::new(),
            },
            linkedin: roomler2_config::OAuthProviderSettings {
                client_id: String::new(),
                client_secret: String::new(),
            },
            microsoft: roomler2_config::OAuthProviderSettings {
                client_id: String::new(),
                client_secret: String::new(),
            },
        },
    }
}

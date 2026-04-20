pub mod dropbox;
pub mod google_drive;
pub mod onedrive;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudFile {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    pub size: u64,
    pub modified_at: Option<String>,
    pub download_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudFolder {
    pub id: String,
    pub name: String,
    pub path: String,
    pub files: Vec<CloudFile>,
    pub subfolders: Vec<CloudFolder>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
}

/// Common trait for cloud storage providers.
#[async_trait]
pub trait CloudStorageProvider: Send + Sync {
    fn provider_name(&self) -> &str;
    fn authorize_url(&self, redirect_uri: &str, state: &str) -> String;
    async fn exchange_code(&self, code: &str, redirect_uri: &str) -> Result<OAuthTokens, String>;
    async fn list_files(
        &self,
        tokens: &OAuthTokens,
        folder_id: Option<&str>,
    ) -> Result<Vec<CloudFile>, String>;
    async fn download_file(&self, tokens: &OAuthTokens, file_id: &str) -> Result<Vec<u8>, String>;
}

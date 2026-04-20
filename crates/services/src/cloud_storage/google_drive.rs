use async_trait::async_trait;
use reqwest::Client;

use super::{CloudFile, CloudStorageProvider, OAuthTokens};

pub struct GoogleDriveService {
    client: Client,
    client_id: String,
    client_secret: String,
}

impl GoogleDriveService {
    pub fn new(client_id: String, client_secret: String) -> Self {
        Self {
            client: Client::new(),
            client_id,
            client_secret,
        }
    }
}

#[async_trait]
impl CloudStorageProvider for GoogleDriveService {
    fn provider_name(&self) -> &str {
        "google_drive"
    }

    fn authorize_url(&self, redirect_uri: &str, state: &str) -> String {
        format!(
            "https://accounts.google.com/o/oauth2/v2/auth?client_id={}&redirect_uri={}&response_type=code&scope=https://www.googleapis.com/auth/drive.readonly&state={}&access_type=offline",
            self.client_id, redirect_uri, state
        )
    }

    async fn exchange_code(&self, code: &str, redirect_uri: &str) -> Result<OAuthTokens, String> {
        let resp = self
            .client
            .post("https://oauth2.googleapis.com/token")
            .form(&[
                ("code", code),
                ("client_id", &self.client_id),
                ("client_secret", &self.client_secret),
                ("redirect_uri", redirect_uri),
                ("grant_type", "authorization_code"),
            ])
            .send()
            .await
            .map_err(|e| format!("Token exchange failed: {}", e))?;

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse token response: {}", e))?;

        Ok(OAuthTokens {
            access_token: json["access_token"].as_str().unwrap_or("").to_string(),
            refresh_token: json["refresh_token"].as_str().map(|s| s.to_string()),
            expires_at: json["expires_in"]
                .as_i64()
                .map(|e| chrono::Utc::now().timestamp() + e),
        })
    }

    async fn list_files(
        &self,
        tokens: &OAuthTokens,
        folder_id: Option<&str>,
    ) -> Result<Vec<CloudFile>, String> {
        let query = match folder_id {
            Some(fid) => format!("'{}' in parents and trashed = false", fid),
            None => "'root' in parents and trashed = false".to_string(),
        };

        let resp = self
            .client
            .get("https://www.googleapis.com/drive/v3/files")
            .bearer_auth(&tokens.access_token)
            .query(&[
                ("q", query.as_str()),
                ("fields", "files(id,name,mimeType,size,modifiedTime)"),
                ("pageSize", "100"),
            ])
            .send()
            .await
            .map_err(|e| format!("List files failed: {}", e))?;

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse files: {}", e))?;

        let files = json["files"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .map(|f| CloudFile {
                id: f["id"].as_str().unwrap_or("").to_string(),
                name: f["name"].as_str().unwrap_or("").to_string(),
                mime_type: f["mimeType"].as_str().unwrap_or("").to_string(),
                size: f["size"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0),
                modified_at: f["modifiedTime"].as_str().map(|s| s.to_string()),
                download_url: None,
            })
            .collect();

        Ok(files)
    }

    async fn download_file(&self, tokens: &OAuthTokens, file_id: &str) -> Result<Vec<u8>, String> {
        let resp = self
            .client
            .get(format!(
                "https://www.googleapis.com/drive/v3/files/{}?alt=media",
                file_id
            ))
            .bearer_auth(&tokens.access_token)
            .send()
            .await
            .map_err(|e| format!("Download failed: {}", e))?;

        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| format!("Failed to read bytes: {}", e))
    }
}

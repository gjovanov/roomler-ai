use async_trait::async_trait;
use reqwest::Client;

use super::{CloudFile, CloudStorageProvider, OAuthTokens};

pub struct DropboxService {
    client: Client,
    app_key: String,
    app_secret: String,
}

impl DropboxService {
    pub fn new(app_key: String, app_secret: String) -> Self {
        Self {
            client: Client::new(),
            app_key,
            app_secret,
        }
    }
}

#[async_trait]
impl CloudStorageProvider for DropboxService {
    fn provider_name(&self) -> &str {
        "dropbox"
    }

    fn authorize_url(&self, redirect_uri: &str, state: &str) -> String {
        format!(
            "https://www.dropbox.com/oauth2/authorize?client_id={}&redirect_uri={}&response_type=code&state={}&token_access_type=offline",
            self.app_key, redirect_uri, state
        )
    }

    async fn exchange_code(&self, code: &str, redirect_uri: &str) -> Result<OAuthTokens, String> {
        let resp = self
            .client
            .post("https://api.dropboxapi.com/oauth2/token")
            .form(&[
                ("code", code),
                ("grant_type", "authorization_code"),
                ("client_id", self.app_key.as_str()),
                ("client_secret", self.app_secret.as_str()),
                ("redirect_uri", redirect_uri),
            ])
            .send()
            .await
            .map_err(|e| format!("Token exchange failed: {}", e))?;

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse token: {}", e))?;

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
        let path = folder_id.unwrap_or("");
        let resp = self
            .client
            .post("https://api.dropboxapi.com/2/files/list_folder")
            .bearer_auth(&tokens.access_token)
            .json(&serde_json::json!({
                "path": if path.is_empty() { "" } else { path },
                "recursive": false,
                "limit": 100,
            }))
            .send()
            .await
            .map_err(|e| format!("List files failed: {}", e))?;

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse: {}", e))?;

        let files = json["entries"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .filter(|e| e[".tag"].as_str() == Some("file"))
            .map(|f| CloudFile {
                id: f["id"].as_str().unwrap_or("").to_string(),
                name: f["name"].as_str().unwrap_or("").to_string(),
                mime_type: "application/octet-stream".to_string(),
                size: f["size"].as_u64().unwrap_or(0),
                modified_at: f["server_modified"].as_str().map(|s| s.to_string()),
                download_url: None,
            })
            .collect();

        Ok(files)
    }

    async fn download_file(&self, tokens: &OAuthTokens, file_id: &str) -> Result<Vec<u8>, String> {
        let resp = self
            .client
            .post("https://content.dropboxapi.com/2/files/download")
            .bearer_auth(&tokens.access_token)
            .header(
                "Dropbox-API-Arg",
                serde_json::json!({ "path": file_id }).to_string(),
            )
            .send()
            .await
            .map_err(|e| format!("Download failed: {}", e))?;

        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| format!("Failed to read bytes: {}", e))
    }
}

use async_trait::async_trait;
use reqwest::Client;

use super::{CloudFile, CloudStorageProvider, OAuthTokens};

pub struct OneDriveService {
    client: Client,
    client_id: String,
    client_secret: String,
}

impl OneDriveService {
    pub fn new(client_id: String, client_secret: String) -> Self {
        Self {
            client: Client::new(),
            client_id,
            client_secret,
        }
    }
}

#[async_trait]
impl CloudStorageProvider for OneDriveService {
    fn provider_name(&self) -> &str {
        "onedrive"
    }

    fn authorize_url(&self, redirect_uri: &str, state: &str) -> String {
        format!(
            "https://login.microsoftonline.com/common/oauth2/v2.0/authorize?client_id={}&redirect_uri={}&response_type=code&scope=Files.Read.All+offline_access&state={}",
            self.client_id, redirect_uri, state
        )
    }

    async fn exchange_code(&self, code: &str, redirect_uri: &str) -> Result<OAuthTokens, String> {
        let resp = self
            .client
            .post("https://login.microsoftonline.com/common/oauth2/v2.0/token")
            .form(&[
                ("code", code),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("redirect_uri", redirect_uri),
                ("grant_type", "authorization_code"),
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
        let url = match folder_id {
            Some(fid) => format!(
                "https://graph.microsoft.com/v1.0/me/drive/items/{}/children",
                fid
            ),
            None => "https://graph.microsoft.com/v1.0/me/drive/root/children".to_string(),
        };

        let resp = self
            .client
            .get(&url)
            .bearer_auth(&tokens.access_token)
            .send()
            .await
            .map_err(|e| format!("List files failed: {}", e))?;

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse: {}", e))?;

        let files = json["value"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .filter(|f| f.get("file").is_some())
            .map(|f| CloudFile {
                id: f["id"].as_str().unwrap_or("").to_string(),
                name: f["name"].as_str().unwrap_or("").to_string(),
                mime_type: f["file"]["mimeType"]
                    .as_str()
                    .unwrap_or("application/octet-stream")
                    .to_string(),
                size: f["size"].as_u64().unwrap_or(0),
                modified_at: f["lastModifiedDateTime"].as_str().map(|s| s.to_string()),
                download_url: f["@microsoft.graph.downloadUrl"]
                    .as_str()
                    .map(|s| s.to_string()),
            })
            .collect();

        Ok(files)
    }

    async fn download_file(&self, tokens: &OAuthTokens, file_id: &str) -> Result<Vec<u8>, String> {
        let resp = self
            .client
            .get(format!(
                "https://graph.microsoft.com/v1.0/me/drive/items/{}/content",
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

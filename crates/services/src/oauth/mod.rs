use roomler2_config::OAuthSettings;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum OAuthError {
    #[error("Provider not configured: {0}")]
    ProviderNotConfigured(String),
    #[error("Unknown provider: {0}")]
    UnknownProvider(String),
    #[error("Token exchange failed: {0}")]
    TokenExchangeFailed(String),
    #[error("User info fetch failed: {0}")]
    UserInfoFailed(String),
    #[error("Invalid state parameter")]
    InvalidState,
}

#[derive(Debug, Clone)]
pub struct OAuthUserInfo {
    pub provider: String,
    pub provider_id: String,
    pub email: String,
    pub name: String,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Debug, Deserialize)]
struct GoogleUser {
    id: String,
    email: Option<String>,
    name: Option<String>,
    given_name: Option<String>,
    family_name: Option<String>,
    picture: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FacebookUser {
    id: String,
    email: Option<String>,
    name: Option<String>,
    picture: Option<FacebookPicture>,
}

#[derive(Debug, Deserialize)]
struct FacebookPicture {
    data: Option<FacebookPictureData>,
}

#[derive(Debug, Deserialize)]
struct FacebookPictureData {
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubUser {
    id: i64,
    login: String,
    email: Option<String>,
    avatar_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubEmail {
    email: String,
    primary: bool,
    verified: bool,
}

#[derive(Debug, Deserialize)]
struct LinkedInUser {
    sub: Option<String>,
    email: Option<String>,
    name: Option<String>,
    given_name: Option<String>,
    family_name: Option<String>,
    picture: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MicrosoftUser {
    id: String,
    display_name: Option<String>,
    given_name: Option<String>,
    mail: Option<String>,
    user_principal_name: Option<String>,
}

pub struct OAuthService {
    settings: OAuthSettings,
    client: reqwest::Client,
}

impl OAuthService {
    pub fn new(settings: OAuthSettings) -> Self {
        Self {
            settings,
            client: reqwest::Client::new(),
        }
    }

    fn provider_config(&self, provider: &str) -> Result<(&str, &str), OAuthError> {
        let cfg = match provider {
            "google" => (&self.settings.google.client_id, &self.settings.google.client_secret),
            "facebook" => (&self.settings.facebook.client_id, &self.settings.facebook.client_secret),
            "github" => (&self.settings.github.client_id, &self.settings.github.client_secret),
            "linkedin" => (&self.settings.linkedin.client_id, &self.settings.linkedin.client_secret),
            "microsoft" => (&self.settings.microsoft.client_id, &self.settings.microsoft.client_secret),
            _ => return Err(OAuthError::UnknownProvider(provider.to_string())),
        };
        if cfg.0.is_empty() {
            return Err(OAuthError::ProviderNotConfigured(provider.to_string()));
        }
        Ok((cfg.0.as_str(), cfg.1.as_str()))
    }

    fn callback_url(&self, provider: &str) -> String {
        format!("{}/api/oauth/callback/{}", self.settings.base_url, provider)
    }

    pub fn build_auth_url(&self, provider: &str, state: &str) -> Result<String, OAuthError> {
        let (client_id, _) = self.provider_config(provider)?;
        let redirect_uri = self.callback_url(provider);

        let url = match provider {
            "google" => format!(
                "https://accounts.google.com/o/oauth2/v2/auth?client_id={}&redirect_uri={}&response_type=code&scope=email+profile&state={}&access_type=offline",
                client_id, urlencoding::encode(&redirect_uri), urlencoding::encode(state)
            ),
            "facebook" => format!(
                "https://www.facebook.com/v18.0/dialog/oauth?client_id={}&redirect_uri={}&response_type=code&scope=email&state={}",
                client_id, urlencoding::encode(&redirect_uri), urlencoding::encode(state)
            ),
            "github" => format!(
                "https://github.com/login/oauth/authorize?client_id={}&redirect_uri={}&scope=user+user:email&state={}",
                client_id, urlencoding::encode(&redirect_uri), urlencoding::encode(state)
            ),
            "linkedin" => format!(
                "https://www.linkedin.com/oauth/v2/authorization?client_id={}&redirect_uri={}&response_type=code&scope=openid+profile+email&state={}",
                client_id, urlencoding::encode(&redirect_uri), urlencoding::encode(state)
            ),
            "microsoft" => format!(
                "https://login.microsoftonline.com/common/oauth2/v2.0/authorize?client_id={}&redirect_uri={}&response_type=code&scope=openid+profile+email+User.Read&state={}",
                client_id, urlencoding::encode(&redirect_uri), urlencoding::encode(state)
            ),
            _ => return Err(OAuthError::UnknownProvider(provider.to_string())),
        };

        Ok(url)
    }

    async fn exchange_code(&self, provider: &str, code: &str) -> Result<String, OAuthError> {
        let (client_id, client_secret) = self.provider_config(provider)?;
        let redirect_uri = self.callback_url(provider);

        let access_token = match provider {
            "google" => {
                let resp = self.client
                    .post("https://oauth2.googleapis.com/token")
                    .form(&[
                        ("code", code),
                        ("client_id", client_id),
                        ("client_secret", client_secret),
                        ("redirect_uri", &redirect_uri),
                        ("grant_type", "authorization_code"),
                    ])
                    .send()
                    .await
                    .map_err(|e| OAuthError::TokenExchangeFailed(e.to_string()))?;
                let body: TokenResponse = resp.json().await
                    .map_err(|e| OAuthError::TokenExchangeFailed(e.to_string()))?;
                body.access_token
            }
            "facebook" => {
                let resp = self.client
                    .get("https://graph.facebook.com/v18.0/oauth/access_token")
                    .query(&[
                        ("code", code),
                        ("client_id", client_id),
                        ("client_secret", client_secret),
                        ("redirect_uri", &redirect_uri),
                    ])
                    .send()
                    .await
                    .map_err(|e| OAuthError::TokenExchangeFailed(e.to_string()))?;
                let body: TokenResponse = resp.json().await
                    .map_err(|e| OAuthError::TokenExchangeFailed(e.to_string()))?;
                body.access_token
            }
            "github" => {
                let resp = self.client
                    .post("https://github.com/login/oauth/access_token")
                    .header("Accept", "application/json")
                    .form(&[
                        ("code", code),
                        ("client_id", client_id),
                        ("client_secret", client_secret),
                        ("redirect_uri", &redirect_uri),
                    ])
                    .send()
                    .await
                    .map_err(|e| OAuthError::TokenExchangeFailed(e.to_string()))?;
                let body: TokenResponse = resp.json().await
                    .map_err(|e| OAuthError::TokenExchangeFailed(e.to_string()))?;
                body.access_token
            }
            "linkedin" => {
                let resp = self.client
                    .post("https://www.linkedin.com/oauth/v2/accessToken")
                    .form(&[
                        ("code", code),
                        ("client_id", client_id),
                        ("client_secret", client_secret),
                        ("redirect_uri", &redirect_uri),
                        ("grant_type", "authorization_code"),
                    ])
                    .send()
                    .await
                    .map_err(|e| OAuthError::TokenExchangeFailed(e.to_string()))?;
                let body: TokenResponse = resp.json().await
                    .map_err(|e| OAuthError::TokenExchangeFailed(e.to_string()))?;
                body.access_token
            }
            "microsoft" => {
                let resp = self.client
                    .post("https://login.microsoftonline.com/common/oauth2/v2.0/token")
                    .form(&[
                        ("code", code),
                        ("client_id", client_id),
                        ("client_secret", client_secret),
                        ("redirect_uri", &redirect_uri),
                        ("grant_type", "authorization_code"),
                    ])
                    .send()
                    .await
                    .map_err(|e| OAuthError::TokenExchangeFailed(e.to_string()))?;
                let body: TokenResponse = resp.json().await
                    .map_err(|e| OAuthError::TokenExchangeFailed(e.to_string()))?;
                body.access_token
            }
            _ => return Err(OAuthError::UnknownProvider(provider.to_string())),
        };

        Ok(access_token)
    }

    async fn fetch_user_info(&self, provider: &str, access_token: &str) -> Result<OAuthUserInfo, OAuthError> {
        match provider {
            "google" => {
                let user: GoogleUser = self.client
                    .get("https://www.googleapis.com/userinfo/v2/me")
                    .bearer_auth(access_token)
                    .send()
                    .await
                    .map_err(|e| OAuthError::UserInfoFailed(e.to_string()))?
                    .json()
                    .await
                    .map_err(|e| OAuthError::UserInfoFailed(e.to_string()))?;
                let name = user.name.unwrap_or_else(|| {
                    format!("{} {}", user.given_name.unwrap_or_default(), user.family_name.unwrap_or_default()).trim().to_string()
                });
                Ok(OAuthUserInfo {
                    provider: "google".to_string(),
                    provider_id: user.id,
                    email: user.email.unwrap_or_default(),
                    name,
                    avatar_url: user.picture,
                })
            }
            "facebook" => {
                let user: FacebookUser = self.client
                    .get("https://graph.facebook.com/v18.0/me?fields=email,name,picture.type(large)")
                    .bearer_auth(access_token)
                    .send()
                    .await
                    .map_err(|e| OAuthError::UserInfoFailed(e.to_string()))?
                    .json()
                    .await
                    .map_err(|e| OAuthError::UserInfoFailed(e.to_string()))?;
                let avatar = user.picture.and_then(|p| p.data).and_then(|d| d.url);
                Ok(OAuthUserInfo {
                    provider: "facebook".to_string(),
                    provider_id: user.id,
                    email: user.email.unwrap_or_default(),
                    name: user.name.unwrap_or_default(),
                    avatar_url: avatar,
                })
            }
            "github" => {
                let user: GitHubUser = self.client
                    .get("https://api.github.com/user")
                    .header("User-Agent", "roomler2")
                    .bearer_auth(access_token)
                    .send()
                    .await
                    .map_err(|e| OAuthError::UserInfoFailed(e.to_string()))?
                    .json()
                    .await
                    .map_err(|e| OAuthError::UserInfoFailed(e.to_string()))?;

                let email = if let Some(email) = user.email {
                    email
                } else {
                    // Fallback: fetch emails endpoint
                    let emails: Vec<GitHubEmail> = self.client
                        .get("https://api.github.com/user/emails")
                        .header("User-Agent", "roomler2")
                        .bearer_auth(access_token)
                        .send()
                        .await
                        .map_err(|e| OAuthError::UserInfoFailed(e.to_string()))?
                        .json()
                        .await
                        .map_err(|e| OAuthError::UserInfoFailed(e.to_string()))?;
                    emails
                        .into_iter()
                        .find(|e| e.primary && e.verified)
                        .map(|e| e.email)
                        .unwrap_or_default()
                };

                Ok(OAuthUserInfo {
                    provider: "github".to_string(),
                    provider_id: user.id.to_string(),
                    email,
                    name: user.login,
                    avatar_url: user.avatar_url,
                })
            }
            "linkedin" => {
                let user: LinkedInUser = self.client
                    .get("https://api.linkedin.com/v2/userinfo")
                    .bearer_auth(access_token)
                    .send()
                    .await
                    .map_err(|e| OAuthError::UserInfoFailed(e.to_string()))?
                    .json()
                    .await
                    .map_err(|e| OAuthError::UserInfoFailed(e.to_string()))?;
                let name = user.name.unwrap_or_else(|| {
                    format!("{} {}", user.given_name.unwrap_or_default(), user.family_name.unwrap_or_default()).trim().to_string()
                });
                Ok(OAuthUserInfo {
                    provider: "linkedin".to_string(),
                    provider_id: user.sub.unwrap_or_default(),
                    email: user.email.unwrap_or_default(),
                    name,
                    avatar_url: user.picture,
                })
            }
            "microsoft" => {
                let user: MicrosoftUser = self.client
                    .get("https://graph.microsoft.com/v1.0/me")
                    .bearer_auth(access_token)
                    .send()
                    .await
                    .map_err(|e| OAuthError::UserInfoFailed(e.to_string()))?
                    .json()
                    .await
                    .map_err(|e| OAuthError::UserInfoFailed(e.to_string()))?;
                let name = user.display_name.or(user.given_name).unwrap_or_default();
                let email = user.mail.or(user.user_principal_name).unwrap_or_default();
                Ok(OAuthUserInfo {
                    provider: "microsoft".to_string(),
                    provider_id: user.id,
                    email,
                    name,
                    avatar_url: None,
                })
            }
            _ => Err(OAuthError::UnknownProvider(provider.to_string())),
        }
    }

    pub async fn authenticate(&self, provider: &str, code: &str) -> Result<OAuthUserInfo, OAuthError> {
        let access_token = self.exchange_code(provider, code).await?;
        self.fetch_user_info(provider, &access_token).await
    }
}

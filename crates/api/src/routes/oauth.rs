use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{error::ApiError, state::AppState};

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: String,
    pub state: String,
}

pub async fn oauth_redirect(
    State(state): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Response, ApiError> {
    let oauth = state.oauth.as_ref().ok_or_else(|| {
        ApiError::BadRequest("OAuth not configured".to_string())
    })?;

    let csrf_state = Uuid::new_v4().to_string();
    // In production, store state in a short-lived cache (Redis) for validation.
    // For now we pass it through and skip strict validation.

    let auth_url = oauth
        .build_auth_url(&provider, &csrf_state)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    Ok(Redirect::temporary(&auth_url).into_response())
}

pub async fn oauth_callback(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Query(params): Query<CallbackQuery>,
) -> Result<Response, ApiError> {
    let oauth = state.oauth.as_ref().ok_or_else(|| {
        ApiError::BadRequest("OAuth not configured".to_string())
    })?;

    // Exchange code and fetch user info
    let user_info = oauth
        .authenticate(&provider, &params.code)
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    if user_info.email.is_empty() {
        return Err(ApiError::BadRequest(
            "Could not retrieve email from OAuth provider".to_string(),
        ));
    }

    // Find or create user
    let user = state
        .users
        .find_or_create_by_oauth(
            &user_info.provider,
            &user_info.provider_id,
            &user_info.email,
            &user_info.name,
            user_info.avatar_url.as_deref(),
        )
        .await?;

    let user_id = user.id.unwrap();

    // Generate JWT tokens
    let tokens = state
        .auth
        .generate_tokens(user_id, &user.email, &user.username)?;

    // Set cookie and redirect to frontend
    let cookie = format!(
        "access_token={}; HttpOnly; Path=/; SameSite=Lax; Max-Age={}",
        tokens.access_token, tokens.expires_in
    );

    let frontend_url = state
        .settings
        .oauth
        .base_url
        .replace(":5001", ":5000"); // API → UI port

    let redirect_url = format!("{}/oauth/callback?token={}", frontend_url, tokens.access_token);

    let mut headers = HeaderMap::new();
    headers.insert(header::SET_COOKIE, cookie.parse().unwrap());
    headers.insert(header::LOCATION, redirect_url.parse().unwrap());

    Ok((StatusCode::FOUND, headers).into_response())
}

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};

#[derive(Debug, Deserialize)]
pub struct SubscribeRequest {
    pub endpoint: String,
    pub keys: PushKeysRequest,
}

#[derive(Debug, Deserialize)]
pub struct PushKeysRequest {
    pub auth: String,
    pub p256dh: String,
}

#[derive(Debug, Deserialize)]
pub struct UnsubscribeRequest {
    pub endpoint: String,
}

#[derive(Debug, Serialize)]
pub struct PushConfigResponse {
    pub vapid_public_key: String,
}

/// GET /push/config — returns the VAPID public key for client-side subscription
pub async fn config(State(state): State<AppState>) -> Result<Json<PushConfigResponse>, ApiError> {
    Ok(Json(PushConfigResponse {
        vapid_public_key: state.settings.push.vapid_public_key.clone(),
    }))
}

/// POST /push/subscribe — register a push subscription for the authenticated user
pub async fn subscribe(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<SubscribeRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .push_subscriptions
        .subscribe(
            auth.user_id,
            body.endpoint,
            body.keys.auth,
            body.keys.p256dh,
        )
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

/// POST /push/unsubscribe — remove a push subscription
pub async fn unsubscribe(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<UnsubscribeRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .push_subscriptions
        .unsubscribe(auth.user_id, &body.endpoint)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

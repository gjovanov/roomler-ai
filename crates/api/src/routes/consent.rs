//! PUBLIC remote-control consent endpoints (Phase 4).
//!
//! A device owner who received an email link / web-push tap lands here to
//! approve or deny a pending remote-control session. There is **no auth
//! extractor** — the unguessable `token` (a `ConsentRequest.token`) IS the
//! capability. The decision resolves the session's in-memory consent slot via
//! `Hub::deliver_consent`, exactly as if the agent (on-host prompt) had replied.

use axum::{
    Json,
    extract::{Path, State},
};
use roomler_ai_db::models::consent_request::ConsentRequestStatus;

use crate::{error::ApiError, state::AppState};

/// `POST /api/consent/{token}/approve`
pub async fn approve_consent(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    resolve(&state, &token, true).await
}

/// `POST /api/consent/{token}/deny`
pub async fn deny_consent(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    resolve(&state, &token, false).await
}

async fn resolve(
    state: &AppState,
    token: &str,
    granted: bool,
) -> Result<Json<serde_json::Value>, ApiError> {
    let req = state
        .consent_requests
        .find_by_token(token)
        .await?
        .ok_or_else(|| ApiError::NotFound("Consent request not found or expired".to_string()))?;

    if req.status != ConsentRequestStatus::Pending {
        return Err(ApiError::BadRequest(
            "This request has already been handled".to_string(),
        ));
    }
    if req.expires_at < bson::DateTime::now() {
        return Err(ApiError::BadRequest("This request has expired".to_string()));
    }

    let id = req
        .id
        .ok_or_else(|| ApiError::Internal("consent request missing id".to_string()))?;
    let status = if granted {
        ConsentRequestStatus::Approved
    } else {
        ConsentRequestStatus::Denied
    };
    // Single-use: only proceed if THIS call flipped Pending → resolved. Guards
    // against a double-click / an approve+deny race replaying the same token.
    let won = state.consent_requests.resolve(id, status).await?;
    if !won {
        return Err(ApiError::BadRequest(
            "This request has already been handled".to_string(),
        ));
    }

    // Resolve the in-memory consent slot. Best-effort: the session may have
    // already timed out or closed, in which case there's nothing to grant — the
    // owner still gets a clean 200 (their decision was recorded).
    if let Err(e) = state.rc_hub.deliver_consent(req.session_id, granted) {
        tracing::info!(
            session = %req.session_id, %e,
            "consent slot no longer waiting (session gone / timed out)"
        );
    }

    Ok(Json(
        serde_json::json!({ "resolved": true, "granted": granted }),
    ))
}

use axum::{
    extract::FromRequestParts,
    http::{header, request::Parts},
};
use bson::oid::ObjectId;
use roomler2_services::auth::Claims;

use crate::{error::ApiError, state::AppState};

/// Extracts the authenticated user from JWT (cookie or Authorization header)
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AuthUser {
    pub user_id: ObjectId,
    pub email: String,
    pub username: String,
    pub claims: Claims,
}

impl<S> FromRequestParts<S> for AuthUser
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app_state = AppState::from_ref(state);

        // Try Authorization header first
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|s| s.to_string())
            // Then try cookie
            .or_else(|| {
                parts
                    .headers
                    .get(header::COOKIE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|cookies| {
                        cookies.split(';').find_map(|cookie| {
                            let cookie = cookie.trim();
                            cookie
                                .strip_prefix("access_token=")
                                .map(|s| s.to_string())
                        })
                    })
            })
            .ok_or_else(|| ApiError::Unauthorized("No token provided".to_string()))?;

        let claims = app_state.auth.verify_access_token(&token)?;

        let user_id = ObjectId::parse_str(&claims.sub)
            .map_err(|_| ApiError::Unauthorized("Invalid user ID in token".to_string()))?;

        Ok(AuthUser {
            user_id,
            email: claims.email.clone(),
            username: claims.username.clone(),
            claims,
        })
    }
}

/// Optional auth extractor — returns `Option<AuthUser>`, never rejects.
/// Use for endpoints that behave differently for authenticated vs unauthenticated users.
pub struct OptionalAuthUser(pub Option<AuthUser>);

impl<S> FromRequestParts<S> for OptionalAuthUser
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Ok(OptionalAuthUser(
            AuthUser::from_request_parts(parts, state).await.ok(),
        ))
    }
}

/// Helper trait for extracting AppState from composite state types
pub trait FromRef<T> {
    fn from_ref(input: &T) -> Self;
}

impl FromRef<AppState> for AppState {
    fn from_ref(input: &AppState) -> Self {
        input.clone()
    }
}

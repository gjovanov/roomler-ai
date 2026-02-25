use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use roomler2_services::dao::base::DaoError;
use roomler2_services::auth::AuthError;

#[derive(Debug)]
pub enum ApiError {
    NotFound(String),
    BadRequest(String),
    Unauthorized(String),
    Forbidden(String),
    Conflict(String),
    Internal(String),
    Validation(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::NotFound(msg) => write!(f, "Not found: {msg}"),
            ApiError::BadRequest(msg) => write!(f, "Bad request: {msg}"),
            ApiError::Unauthorized(msg) => write!(f, "Unauthorized: {msg}"),
            ApiError::Forbidden(msg) => write!(f, "Forbidden: {msg}"),
            ApiError::Conflict(msg) => write!(f, "Conflict: {msg}"),
            ApiError::Internal(msg) => write!(f, "Internal error: {msg}"),
            ApiError::Validation(msg) => write!(f, "Validation: {msg}"),
        }
    }
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
    message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, error_type, message) = match self {
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, "not_found", msg),
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "bad_request", msg),
            ApiError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, "unauthorized", msg),
            ApiError::Forbidden(msg) => (StatusCode::FORBIDDEN, "forbidden", msg),
            ApiError::Conflict(msg) => (StatusCode::CONFLICT, "conflict", msg),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, "internal", msg),
            ApiError::Validation(msg) => (StatusCode::UNPROCESSABLE_ENTITY, "validation", msg),
        };

        let body = ErrorResponse {
            error: error_type.to_string(),
            message,
        };

        (status, Json(body)).into_response()
    }
}

impl From<DaoError> for ApiError {
    fn from(err: DaoError) -> Self {
        match err {
            DaoError::NotFound => ApiError::NotFound("Resource not found".to_string()),
            DaoError::DuplicateKey(msg) => ApiError::Conflict(msg),
            DaoError::Forbidden(msg) => ApiError::Forbidden(msg),
            DaoError::Validation(msg) => ApiError::Validation(msg),
            DaoError::Mongo(e) => ApiError::Internal(e.to_string()),
            DaoError::BsonSer(e) => ApiError::Internal(e.to_string()),
            DaoError::BsonDe(e) => ApiError::Internal(e.to_string()),
        }
    }
}

impl From<AuthError> for ApiError {
    fn from(err: AuthError) -> Self {
        match err {
            AuthError::InvalidCredentials => {
                ApiError::Unauthorized("Invalid credentials".to_string())
            }
            AuthError::TokenExpired => ApiError::Unauthorized("Token expired".to_string()),
            AuthError::InvalidToken(msg) => ApiError::Unauthorized(msg),
            AuthError::HashError(msg) => ApiError::Internal(msg),
        }
    }
}

impl From<roomler2_services::oauth::OAuthError> for ApiError {
    fn from(err: roomler2_services::oauth::OAuthError) -> Self {
        match err {
            roomler2_services::oauth::OAuthError::ProviderNotConfigured(msg) => {
                ApiError::BadRequest(format!("Provider not configured: {msg}"))
            }
            roomler2_services::oauth::OAuthError::UnknownProvider(msg) => {
                ApiError::BadRequest(format!("Unknown provider: {msg}"))
            }
            roomler2_services::oauth::OAuthError::InvalidState => {
                ApiError::BadRequest("Invalid OAuth state".to_string())
            }
            other => ApiError::Internal(other.to_string()),
        }
    }
}

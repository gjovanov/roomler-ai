// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use axum::{
    Json,
    extract::rejection::JsonRejection,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use roomler_ai_services::auth::AuthError;
use roomler_ai_services::dao::base::{DaoError, duplicate_key_site};
use serde::Serialize;

/// The answer to a collision no route put in words. A route that knows what
/// collided says so itself; every other one answers this, in place of MongoDB's
/// text, which names the database, the collection, the index and the value.
const DUPLICATE_KEY_ANSWER: &str = "That already exists";

#[derive(Debug)]
pub enum ApiError {
    NotFound(String),
    BadRequest(String),
    Unauthorized(String),
    Forbidden(String),
    /// 403, but specifically "you are not in this tenant" — as opposed to
    /// "you are, and you may not do that" ([`ApiError::Forbidden`]).
    ///
    /// FR-82 — the two are the same status code and were the same string, so
    /// the SPA could not tell them apart and treated every 403 as a dead
    /// session: it wiped the login and bounced to `/login`. That is wrong for
    /// both halves — a permission refusal is an answer, not an expired
    /// credential — but only ONE of them has a sensible navigation, which is
    /// why the distinction has to reach the client instead of being inferred
    /// there. Serialises as `error: "not_a_member"`; the client leaves the
    /// tenant and keeps the session.
    ///
    /// ⚠️ TENANT membership only. A room refusal is not this — `chat`'s
    /// "Not a member of this room" stays a plain `Forbidden`, because
    /// leaving the org over a private channel would be absurd.
    NotAMember,
    Conflict(String),
    Internal(String),
    Validation(String),
    ServiceUnavailable(String),
    /// A request body axum's `Json` extractor refused, answered exactly as
    /// the extractor answers it: its own status (400 for a body that is not
    /// JSON, 413 past the size limit, 415 for one not sent as JSON, 422 for
    /// one of the wrong shape) and its own plain-text words.
    ///
    /// #4187 — for a route whose permission is decided before its body is
    /// read. Such a route takes `Result<Json<T>, JsonRejection>` in place of
    /// `Json<T>`, so axum refuses no body before the handler runs, and reads
    /// it with `let Json(body) = body?;` once its gate has answered. A caller
    /// the gate refuses gets that refusal whatever the body is; a caller it
    /// lets through gets the answer the extractor gave before.
    JsonBody(JsonRejection),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::NotFound(msg) => write!(f, "Not found: {msg}"),
            ApiError::BadRequest(msg) => write!(f, "Bad request: {msg}"),
            ApiError::Unauthorized(msg) => write!(f, "Unauthorized: {msg}"),
            ApiError::Forbidden(msg) => write!(f, "Forbidden: {msg}"),
            ApiError::NotAMember => write!(f, "Forbidden: Not a member"),
            ApiError::Conflict(msg) => write!(f, "Conflict: {msg}"),
            ApiError::Internal(msg) => write!(f, "Internal error: {msg}"),
            ApiError::Validation(msg) => write!(f, "Validation: {msg}"),
            ApiError::ServiceUnavailable(msg) => write!(f, "Service unavailable: {msg}"),
            ApiError::JsonBody(rejection) => write!(f, "Refused body: {}", rejection.body_text()),
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
        // Log the cause of every 5xx into the structured logs so a
        // production 500 can be diagnosed from `kubectl logs` without
        // needing the user to capture the response body. tower_http's
        // trace layer logs `status=500` but discards the body, leaving
        // a recurring "API error 500" report unfixable until someone
        // pastes the response payload. Other status classes use
        // existing tracing of their own (auth middleware logs 401
        // reasons, validation logs 422 fields, etc.).
        if let ApiError::Internal(msg) = &self {
            tracing::error!(message = %msg, "ApiError::Internal -> 500");
        }
        let (status, error_type, message) = match self {
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, "not_found", msg),
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "bad_request", msg),
            ApiError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, "unauthorized", msg),
            ApiError::Forbidden(msg) => (StatusCode::FORBIDDEN, "forbidden", msg),
            ApiError::NotAMember => (
                StatusCode::FORBIDDEN,
                "not_a_member",
                "Not a member".to_string(),
            ),
            ApiError::Conflict(msg) => (StatusCode::CONFLICT, "conflict", msg),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, "internal", msg),
            ApiError::Validation(msg) => (StatusCode::UNPROCESSABLE_ENTITY, "validation", msg),
            ApiError::ServiceUnavailable(msg) => {
                (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable", msg)
            }
            // The extractor's own answer, not the `{error, message}` shape:
            // the words a caller got while the extractor refused the body
            // before the handler ran (#4187).
            ApiError::JsonBody(rejection) => return rejection.into_response(),
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
            DaoError::DuplicateKey(msg) => {
                tracing::info!(
                    site = duplicate_key_site(&msg),
                    "DaoError::DuplicateKey -> 409"
                );
                ApiError::Conflict(DUPLICATE_KEY_ANSWER.to_string())
            }
            DaoError::Conflict(msg) => ApiError::Conflict(msg),
            DaoError::Forbidden(msg) => ApiError::Forbidden(msg),
            DaoError::NotAMember => ApiError::NotAMember,
            DaoError::Validation(msg) => ApiError::Validation(msg),
            DaoError::Mongo(e) => ApiError::Internal(e.to_string()),
            DaoError::BsonSer(e) => ApiError::Internal(e.to_string()),
            DaoError::BsonDe(e) => ApiError::Internal(e.to_string()),
        }
    }
}

/// #4187 — `?` on a body taken as `Result<Json<T>, JsonRejection>` answers
/// as the extractor would have ([`ApiError::JsonBody`]).
impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        ApiError::JsonBody(rejection)
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

/// #3328 — what every enrollment door answers when the token's one use cannot
/// be spent, in one place so the doors cannot drift apart. The two stay
/// different answers on purpose: "already used" sends a person looking for
/// whoever used the token, and a ledger that could not answer must not borrow
/// those words. Its 503 is retryable, and the token is still valid.
impl From<roomler_ai_services::auth::SpendRefusal> for ApiError {
    fn from(refusal: roomler_ai_services::auth::SpendRefusal) -> Self {
        use roomler_ai_services::auth::SpendRefusal;
        match refusal {
            SpendRefusal::AlreadyUsed => {
                ApiError::Unauthorized("This enrollment token has already been used".to_string())
            }
            SpendRefusal::LedgerUnavailable => ApiError::ServiceUnavailable(
                "Could not verify this enrollment token is unused; please retry".to_string(),
            ),
        }
    }
}

impl From<roomler_ai_services::oauth::OAuthError> for ApiError {
    fn from(err: roomler_ai_services::oauth::OAuthError) -> Self {
        match err {
            roomler_ai_services::oauth::OAuthError::ProviderNotConfigured(msg) => {
                ApiError::BadRequest(format!("Provider not configured: {msg}"))
            }
            roomler_ai_services::oauth::OAuthError::UnknownProvider(msg) => {
                ApiError::BadRequest(format!("Unknown provider: {msg}"))
            }
            roomler_ai_services::oauth::OAuthError::InvalidState => {
                ApiError::BadRequest("Invalid OAuth state".to_string())
            }
            other => ApiError::Internal(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &str = "E11000 duplicate key error collection: roomler.users \
index: email_1 dup key: { email: \"victim@corp.example\" }";

    fn conflict_text(err: DaoError) -> String {
        match ApiError::from(err) {
            ApiError::Conflict(msg) => msg,
            other => panic!("expected a 409, got {other}"),
        }
    }

    #[test]
    fn mongos_text_never_becomes_the_answer() {
        assert_eq!(
            conflict_text(DaoError::DuplicateKey(RAW.to_string())),
            DUPLICATE_KEY_ANSWER
        );
        // A sentence that embeds it is no different: the OAuth retry loop's.
        let wrapped = format!("no username after 5 attempts; last collision: {RAW}");
        assert_eq!(
            conflict_text(DaoError::DuplicateKey(wrapped)),
            DUPLICATE_KEY_ANSWER
        );
    }

    /// #3328 — every enrollment door answers a refused spend in these two
    /// sentences, and they stay two: 401 for a token used before, a retryable
    /// 503 for a ledger that could not answer.
    #[test]
    fn a_refused_spend_keeps_already_used_apart_from_retry() {
        use roomler_ai_services::auth::SpendRefusal;
        match ApiError::from(SpendRefusal::AlreadyUsed) {
            ApiError::Unauthorized(msg) => {
                assert_eq!(msg, "This enrollment token has already been used")
            }
            other => panic!("expected a 401, got {other}"),
        }
        match ApiError::from(SpendRefusal::LedgerUnavailable) {
            ApiError::ServiceUnavailable(msg) => assert_eq!(
                msg,
                "Could not verify this enrollment token is unused; please retry"
            ),
            other => panic!("expected a 503, got {other}"),
        }
    }

    #[test]
    fn a_sentence_written_for_the_person_is_their_answer() {
        assert_eq!(
            conflict_text(DaoError::Conflict("Already reacted".to_string())),
            "Already reacted"
        );
    }

    /// #4187 — a body axum's `Json` extractor refused is answered exactly as
    /// the extractor answers it, whatever the kind: the same status, the same
    /// `Content-Type` and the same words. A route that reads its body after
    /// its gate tells a caller the gate lets through what the extractor told
    /// them before.
    #[tokio::test]
    async fn a_refused_body_is_answered_as_the_json_extractor_answers_it() {
        use axum::extract::FromRequest;
        use std::collections::HashMap;

        async fn refused(content_type: &str, body: &'static str) -> JsonRejection {
            let request = axum::http::Request::builder()
                .header(axum::http::header::CONTENT_TYPE, content_type)
                .body(axum::body::Body::from(body))
                .unwrap();
            match <Json<HashMap<String, String>> as FromRequest<()>>::from_request(request, &())
                .await
            {
                Ok(_) => panic!("{body:?} as {content_type} parses"),
                Err(rejection) => rejection,
            }
        }

        async fn answer(response: Response) -> (StatusCode, Option<String>, Vec<u8>) {
            let content_type = response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .map(|v| v.to_str().unwrap().to_string());
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec();
            (status, content_type, body)
        }

        for (content_type, body, status) in [
            ("application/json", "{", StatusCode::BAD_REQUEST),
            (
                "application/json",
                r#"{"name": 7}"#,
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                "text/plain",
                r#"{"name": "x"}"#,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ),
        ] {
            let ours =
                answer(ApiError::from(refused(content_type, body).await).into_response()).await;
            let extractors = answer(refused(content_type, body).await.into_response()).await;
            assert_eq!(ours, extractors, "{body:?} as {content_type}");
            assert_eq!(ours.0, status, "{body:?} as {content_type}");
        }
    }
}

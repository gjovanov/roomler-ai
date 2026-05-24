//! rc.58 — centralized log-batch ingest + listing.
//!
//! Two ingest endpoints:
//! - `POST /api/tenant/{tenant_id}/agent/{agent_id}/logs` (auth: agent
//!   JWT; sources: agent / service / installer / crash / updater).
//! - `POST /api/log/browser` (auth: user JWT; source: browser).
//!
//! Both validate the body shape, server-stamp `created_at`, then call
//! [`AgentLogDao::record_batch`].
//!
//! Listing endpoint (admin UI): `GET
//! /api/tenant/{tenant_id}/agent/{agent_id}/logs?limit=N` — standard
//! `AuthUser` + tenant-membership check.
//!
//! Server-side scrub: defense-in-depth strip of JWT-shaped tokens from
//! every `msg` field at ingest time. The agent should never leak a
//! token (per ed69c6c rc.53 PII rules) but this re-scrubs.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::IntoResponse,
};
use bson::{DateTime, oid::ObjectId};
use roomler_ai_db::models::{AgentLogBatch, LogLine, LogSource};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};

/// Body shape POSTed by an uploader. `source` must NOT be `Browser`
/// on the agent-authed route; route handler enforces this.
#[derive(Debug, Deserialize)]
pub struct LogBatchPayload {
    pub source: LogSource,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub host_id_hash: Option<String>,
    #[serde(default)]
    pub agent_version: Option<String>,
    pub lines: Vec<LogLine>,
}

#[derive(Debug, Deserialize)]
pub struct ListLogsQuery {
    #[serde(default = "default_limit")]
    pub limit: i64,
}

fn default_limit() -> i64 {
    50
}

#[derive(Debug, Serialize)]
pub struct LogsListResponse {
    pub batches: Vec<AgentLogBatchView>,
}

/// View-model for an admin-UI log batch row. Mirrors the wire shape
/// but exposes `id` as hex string + `created_at` as RFC 3339 so the
/// SPA doesn't need to handle BSON dates.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentLogBatchView {
    pub id: String,
    pub source: String,
    pub agent_id: Option<String>,
    pub user_id: Option<String>,
    pub session_id: Option<String>,
    pub host_id_hash: Option<String>,
    pub agent_version: Option<String>,
    pub line_count: u32,
    pub created_at: String,
    pub lines: Vec<LogLine>,
}

impl From<AgentLogBatch> for AgentLogBatchView {
    fn from(b: AgentLogBatch) -> Self {
        Self {
            id: b.id.map(|o| o.to_hex()).unwrap_or_default(),
            source: b.source.as_str().to_string(),
            agent_id: b.agent_id.map(|o| o.to_hex()),
            user_id: b.user_id.map(|o| o.to_hex()),
            session_id: b.session_id,
            host_id_hash: b.host_id_hash,
            agent_version: b.agent_version,
            line_count: b.line_count,
            created_at: b.created_at.try_to_rfc3339_string().unwrap_or_default(),
            lines: b.lines,
        }
    }
}

/// POST `/api/tenant/{tenant_id}/agent/{agent_id}/logs` — ingest a
/// batch from the agent-side uploader. Auth: agent JWT. The JWT's
/// `tenant_id` + `sub` must match the route params (cross-tenant /
/// cross-agent uploads are rejected as 403).
///
/// Status codes:
///   201 — accepted, batch persisted.
///   401 — missing / malformed / invalid agent JWT.
///   403 — agent-id or tenant-id in route doesn't match JWT claims.
///   422 — payload validation failed (too many lines, oversized msg,
///         body too large, browser source on agent route).
///   500 — DB write failure.
pub async fn ingest_agent(
    State(state): State<AppState>,
    Path((tenant_id, agent_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(payload): Json<LogBatchPayload>,
) -> Result<impl IntoResponse, ApiError> {
    let token = extract_bearer(&headers)
        .ok_or_else(|| ApiError::Unauthorized("Missing Authorization header".to_string()))?;
    let claims = state
        .auth
        .verify_agent_token(token)
        .map_err(|e| ApiError::Unauthorized(e.to_string()))?;

    // Route params must match JWT claims — defense-in-depth against
    // route-config mistakes that would let agent A in tenant T upload
    // batches under agent B / tenant T'. JWT validation alone doesn't
    // catch this since the agent's JWT trusts the issuer.
    if claims.tenant_id != tenant_id {
        return Err(ApiError::Forbidden(
            "tenant_id in route does not match agent JWT".to_string(),
        ));
    }
    if claims.sub != agent_id {
        return Err(ApiError::Forbidden(
            "agent_id in route does not match agent JWT".to_string(),
        ));
    }

    // Source must be agent-side. Browser source has its own user-
    // authed route below.
    if payload.source == LogSource::Browser {
        return Err(ApiError::Validation(
            "browser source not accepted on agent-authed route".to_string(),
        ));
    }

    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("invalid tenant_id".to_string()))?;
    let aid = ObjectId::parse_str(&agent_id)
        .map_err(|_| ApiError::BadRequest("invalid agent_id".to_string()))?;

    let batch = build_batch(tid, Some(aid), None, payload)?;
    state
        .agent_logs
        .record_batch(batch)
        .await
        .map_err(|e| ApiError::Internal(format!("agent_logs insert: {e}")))?;

    Ok((StatusCode::CREATED, Json(json!({ "status": "accepted" }))))
}

/// POST `/api/log/browser` — ingest a batch from the browser console
/// uploader. Auth: user JWT. The user's active tenant is resolved
/// from a `tenant_id` query / form param (not yet wired — punted to
/// rc.59 where the browser-side composable lands). For now the body
/// must include `tenant_id`.
pub async fn ingest_browser(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(payload): Json<BrowserLogBatchPayload>,
) -> Result<impl IntoResponse, ApiError> {
    let tid = ObjectId::parse_str(&payload.tenant_id)
        .map_err(|_| ApiError::BadRequest("invalid tenant_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a tenant member".to_string()));
    }

    // Browser route only accepts Browser source.
    if payload.inner.source != LogSource::Browser {
        return Err(ApiError::Validation(
            "non-browser source not accepted on browser route".to_string(),
        ));
    }

    let batch = build_batch(tid, None, Some(auth.user_id), payload.inner)?;
    state
        .agent_logs
        .record_batch(batch)
        .await
        .map_err(|e| ApiError::Internal(format!("agent_logs insert: {e}")))?;

    Ok((StatusCode::CREATED, Json(json!({ "status": "accepted" }))))
}

/// Browser-route body — wraps a [`LogBatchPayload`] with an explicit
/// `tenant_id` since the user JWT alone doesn't pin a tenant.
#[derive(Debug, Deserialize)]
pub struct BrowserLogBatchPayload {
    pub tenant_id: String,
    #[serde(flatten)]
    pub inner: LogBatchPayload,
}

/// GET `/api/tenant/{tenant_id}/agent/{agent_id}/logs?limit=N` —
/// list the most-recent N batches. Auth: standard user JWT + tenant
/// membership. Default limit: 50.
pub async fn list_for_agent(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((tenant_id, agent_id)): Path<(String, String)>,
    Query(q): Query<ListLogsQuery>,
) -> Result<Json<LogsListResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let aid = ObjectId::parse_str(&agent_id)
        .map_err(|_| ApiError::BadRequest("Invalid agent_id".to_string()))?;

    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let limit = q.limit.clamp(1, 500);
    let records = state
        .agent_logs
        .list_recent_for_agent(tid, aid, limit)
        .await
        .map_err(|e| ApiError::Internal(format!("agent_logs list: {e}")))?;

    let batches: Vec<AgentLogBatchView> = records.into_iter().map(Into::into).collect();
    Ok(Json(LogsListResponse { batches }))
}

/// Shared builder that validates + stamps server-side fields.
fn build_batch(
    tenant_id: ObjectId,
    agent_id: Option<ObjectId>,
    user_id: Option<ObjectId>,
    mut payload: LogBatchPayload,
) -> Result<AgentLogBatch, ApiError> {
    if payload.lines.len() > AgentLogBatch::MAX_LINES_PER_BATCH {
        return Err(ApiError::Validation(format!(
            "lines exceeds {} per batch",
            AgentLogBatch::MAX_LINES_PER_BATCH
        )));
    }
    for line in payload.lines.iter() {
        if line.msg.len() > AgentLogBatch::MAX_MSG_BYTES {
            return Err(ApiError::Validation(format!(
                "msg exceeds {} bytes",
                AgentLogBatch::MAX_MSG_BYTES
            )));
        }
    }
    // Defense-in-depth scrub for JWT-shaped tokens (agent SHOULD have
    // already scrubbed at source; this catches accidental regressions).
    for line in payload.lines.iter_mut() {
        line.msg = scrub_tokens(&line.msg);
    }
    Ok(AgentLogBatch {
        id: None,
        tenant_id,
        source: payload.source,
        agent_id,
        user_id,
        session_id: payload.session_id,
        host_id_hash: payload.host_id_hash,
        agent_version: payload.agent_version,
        line_count: payload.lines.len() as u32,
        created_at: DateTime::now(),
        lines: payload.lines,
    })
}

/// Strip JWT-shaped substrings (`xxx.yyy.zzz`) and `Bearer <token>`
/// patterns from a string. Returns a NEW String; if no match, returns
/// the original.
fn scrub_tokens(s: &str) -> String {
    // Cheap path: most lines don't look like JWTs. Skip the regex for
    // the 99.9% case.
    if !s.contains("eyJ") && !s.contains("Bearer ") && !s.contains("bearer ") {
        return s.to_string();
    }
    let mut out = s.to_string();
    // JWT: three base64url-ish segments separated by dots, starting
    // with "eyJ" (the magic byte for a JSON-encoded header).
    // Conservative: only replace tokens at least 64 chars total (real
    // JWTs are 200+ chars; below 64 is likely a false positive).
    let bytes = out.as_bytes();
    let mut i = 0;
    let mut replacements: Vec<(usize, usize)> = Vec::new();
    while i + 3 <= bytes.len() {
        if &bytes[i..i + 3] == b"eyJ" {
            // Find run end: chars in [A-Za-z0-9._-].
            let mut j = i + 3;
            let mut dots = 0;
            while j < bytes.len() {
                let c = bytes[j];
                if c == b'.' {
                    dots += 1;
                } else if !c.is_ascii_alphanumeric() && c != b'_' && c != b'-' {
                    break;
                }
                j += 1;
            }
            if dots >= 2 && j - i >= 64 {
                replacements.push((i, j));
            }
            i = j;
        } else {
            i += 1;
        }
    }
    // Apply in reverse to keep indices valid.
    for (start, end) in replacements.iter().rev() {
        out.replace_range(*start..*end, "[REDACTED_JWT]");
    }
    // Handle Bearer + bearer prefixes followed by long-ish tokens.
    for prefix in &["Bearer ", "bearer "] {
        while let Some(pos) = out.find(prefix) {
            let after = pos + prefix.len();
            let bytes = out.as_bytes();
            let mut j = after;
            while j < bytes.len() {
                let c = bytes[j];
                if !c.is_ascii_alphanumeric() && c != b'.' && c != b'_' && c != b'-' {
                    break;
                }
                j += 1;
            }
            if j - after >= 8 {
                out.replace_range(pos..j, "[REDACTED_BEARER]");
            } else {
                // Short suffix; advance past prefix to avoid infinite loop.
                let mut tmp = out;
                tmp.replace_range(pos..pos + prefix.len() - 1, "_");
                out = tmp;
            }
        }
    }
    out
}

fn extract_bearer(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let token = v
        .strip_prefix("Bearer ")
        .or_else(|| v.strip_prefix("bearer "))?;
    let trimmed = token.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roomler_ai_db::models::LogLevel;

    fn sample_line(msg: &str) -> LogLine {
        LogLine {
            ts: DateTime::now(),
            level: LogLevel::Info,
            target: "test".to_string(),
            msg: msg.to_string(),
            fields: bson::doc! {},
        }
    }

    fn sample_payload(source: LogSource, lines: Vec<LogLine>) -> LogBatchPayload {
        LogBatchPayload {
            source,
            session_id: Some("69ab".to_string()),
            host_id_hash: Some("abc123".to_string()),
            agent_version: Some("0.3.0-rc.58".to_string()),
            lines,
        }
    }

    #[test]
    fn build_batch_stamps_server_fields() {
        let tid = ObjectId::new();
        let aid = ObjectId::new();
        let batch = build_batch(
            tid,
            Some(aid),
            None,
            sample_payload(LogSource::Agent, vec![sample_line("hello")]),
        )
        .expect("build_batch");
        assert_eq!(batch.tenant_id, tid);
        assert_eq!(batch.agent_id, Some(aid));
        assert_eq!(batch.user_id, None);
        assert_eq!(batch.line_count, 1);
        assert_eq!(batch.source, LogSource::Agent);
    }

    #[test]
    fn build_batch_rejects_oversized_batch() {
        let tid = ObjectId::new();
        let lines: Vec<LogLine> = (0..(AgentLogBatch::MAX_LINES_PER_BATCH + 1))
            .map(|_| sample_line("x"))
            .collect();
        let err = build_batch(tid, None, None, sample_payload(LogSource::Browser, lines))
            .expect_err("should reject");
        assert!(matches!(err, ApiError::Validation(_)));
    }

    #[test]
    fn build_batch_rejects_oversized_msg() {
        let tid = ObjectId::new();
        let big_msg = "x".repeat(AgentLogBatch::MAX_MSG_BYTES + 1);
        let payload = sample_payload(LogSource::Agent, vec![sample_line(&big_msg)]);
        let err = build_batch(tid, None, None, payload).expect_err("should reject");
        assert!(matches!(err, ApiError::Validation(_)));
    }

    #[test]
    fn build_batch_scrubs_jwt_in_msg() {
        let tid = ObjectId::new();
        // Realistic JWT shape — three base64url segments, ~200 chars total.
        let jwt = "eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMiLCJpYXQiOjE1MTYyMzkwMjJ9.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let msg = format!("connecting with token {jwt} ok");
        let payload = sample_payload(LogSource::Agent, vec![sample_line(&msg)]);
        let batch =
            build_batch(tid, None, None, payload).expect("build_batch should succeed after scrub");
        assert!(
            !batch.lines[0].msg.contains(jwt),
            "raw JWT should not survive scrub: {}",
            batch.lines[0].msg
        );
        assert!(batch.lines[0].msg.contains("[REDACTED_JWT]"));
    }

    #[test]
    fn build_batch_scrubs_bearer_header() {
        let tid = ObjectId::new();
        let msg = "header was: Bearer somelongtokenvaluehere1234567890";
        let payload = sample_payload(LogSource::Agent, vec![sample_line(msg)]);
        let batch = build_batch(tid, None, None, payload).expect("build_batch");
        assert!(batch.lines[0].msg.contains("[REDACTED_BEARER]"));
        assert!(!batch.lines[0].msg.contains("somelongtokenvaluehere"));
    }

    #[test]
    fn scrub_tokens_passes_through_non_jwt() {
        let s = "just a normal log line with no secrets";
        assert_eq!(scrub_tokens(s), s);
    }

    #[test]
    fn scrub_tokens_ignores_short_eyj_prefix() {
        // "eyJ" appears in non-JWT contexts too (e.g. base64-encoded
        // text that happens to start with these chars). Don't scrub
        // unless the full JWT shape (>=64 chars + 2 dots) is present.
        let s = "saw eyJabc not a real jwt";
        assert_eq!(scrub_tokens(s), s);
    }

    #[test]
    fn extract_bearer_extracts_token() {
        use axum::http::HeaderValue;
        let mut h = HeaderMap::new();
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer abc.def.ghi"),
        );
        assert_eq!(extract_bearer(&h), Some("abc.def.ghi"));
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
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
//! /api/tenant/{tenant_id}/agent/{agent_id}/logs?limit=N` — `AuthUser` +
//! `MANAGE_AGENTS` in the path tenant ([`crate::diagnostics::require_reader`]).
//!
//! Server-side scrub: defence in depth over EVERY string a line carries
//! (`msg`, `target`, each string in `fields`), at ingest time
//! ([`crate::diagnostics`]). The agent should never log a credential (per
//! ed69c6c rc.53 PII rules); this is the second line.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use bson::{DateTime, oid::ObjectId};
use roomler_ai_db::models::{AgentLogBatch, LogLine, LogSource};
use serde::{Deserialize, Serialize};
use serde_json::json;

use roomler_core::{ApiError, extractors::auth::AuthUser};

use crate::{FleetState, auth_agent::AuthAgent, diagnostics};

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
///   401 — missing / malformed / invalid agent JWT, or an agent whose row
///         has been deleted or quarantined.
///   403 — agent-id or tenant-id in route doesn't match JWT claims.
///   422 — payload validation failed (too many lines, oversized msg,
///         body too large, browser source on agent route).
///   500 — DB write failure, or the agent row could not be read.
///
/// The `AuthAgent` extractor is what makes the 401 case real: this handler
/// used to trust the claims alone, so a deleted or quarantined device kept
/// uploading logs for the remaining life of its year-long token.
pub async fn ingest_agent(
    State(state): State<FleetState>,
    Path((tenant_id, agent_id)): Path<(String, String)>,
    agent: AuthAgent,
    Json(payload): Json<LogBatchPayload>,
) -> Result<impl IntoResponse, ApiError> {
    // Route params must match JWT claims — defense-in-depth against
    // route-config mistakes that would let agent A in tenant T upload
    // batches under agent B / tenant T'. JWT validation alone doesn't
    // catch this since the agent's JWT trusts the issuer.
    //
    // Compare against the EXTRACTOR's ids, not the raw claims: those are the
    // ones that were looked up and accepted, so the check cannot pass against
    // an identity the row check never saw.
    if agent.tenant_id.to_hex() != tenant_id {
        return Err(ApiError::Forbidden(
            "tenant_id in route does not match agent JWT".to_string(),
        ));
    }
    if agent.agent_id.to_hex() != agent_id {
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

    let batch = build_batch(agent.tenant_id, Some(agent.agent_id), None, payload)?;
    state
        .agent_logs
        .record_batch(batch)
        .await
        .map_err(|e| ApiError::Internal(format!("agent_logs insert: {e}")))?;

    Ok((StatusCode::CREATED, Json(json!({ "status": "accepted" }))))
}

/// POST `/api/log/browser` — ingest a batch from the browser. Auth: user
/// JWT; the body names the `tenant_id`, and the caller must be a member of
/// it (the user JWT alone pins no tenant).
///
/// First caller, FR-22 part 4 (2026-09-25): the RC viewer's connect-timing
/// record — one batch of one `rc.connect` line per connect attempt
/// (`ui/src/composables/rcConnectTimingUpload.ts`). The route had existed
/// since rc.58 with no caller at all, so `agent_logs` held zero browser rows
/// for as long as it had a browser source. The exact JSON that caller emits
/// is locked to parse here by `browser_connect_timing_record_parses`.
pub async fn ingest_browser(
    State(state): State<FleetState>,
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
/// list the most-recent N batches. Auth: user JWT and `MANAGE_AGENTS` in
/// the path tenant, then the device resolved within it (removed devices
/// included): [`crate::diagnostics::require_reader`]. Default limit: 50.
pub async fn list_for_agent(
    State(state): State<FleetState>,
    auth: AuthUser,
    Path((tenant_id, agent_id)): Path<(String, String)>,
    Query(q): Query<ListLogsQuery>,
) -> Result<Json<LogsListResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;
    let aid = ObjectId::parse_str(&agent_id)
        .map_err(|_| ApiError::BadRequest("Invalid agent_id".to_string()))?;

    diagnostics::require_reader(&state, tid, aid, auth.user_id).await?;

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
    // Defence in depth: the agent SHOULD have scrubbed at source, but a
    // device that predates a device-side fix sends what it had, in any
    // field. So every string a line carries is scrubbed: `msg`, `target`,
    // and each string in `fields`.
    for line in payload.lines.iter_mut() {
        diagnostics::scrub_string(&mut line.msg);
        diagnostics::scrub_string(&mut line.target);
        diagnostics::scrub_document(&mut line.fields);
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

    /// The bytes 0x00..=0x1f in standard base64: a WireGuard key's shape.
    const WG_KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
    const JWT: &str = "eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMiLCJpYXQiOjE1MTYyMzkwMjJ9.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";

    /// Every string a line carries is scrubbed, `fields` at any depth included:
    /// a structured field holds whatever the code logged there (a parser's
    /// error, say), and a browser batch can nest documents and arrays.
    #[test]
    fn build_batch_scrubs_every_string_a_line_carries() {
        let toml_error = format!(
            "parse /etc/roomler/config.toml: TOML parse error at line 7, column 71\n  |\n7 | overlay_wg_secret_key = \"{WG_KEY}\"x\n  |{}^\nexpected newline, `#`\n",
            " ".repeat(71)
        );
        let mut line = sample_line(&format!("overlay key {WG_KEY} refused"));
        line.target = format!("roomlerd::{JWT}");
        line.fields = bson::doc! {
            "error": toml_error,
            "token": JWT,
            "nested": { "auth": format!("Bearer {JWT}"), "keys": [WG_KEY, "kept"] },
            "n": 3_i64,
        };
        let batch = build_batch(
            ObjectId::new(),
            None,
            None,
            sample_payload(LogSource::Agent, vec![line]),
        )
        .expect("build_batch");
        let stored = format!("{:?}", batch.lines[0]);
        assert!(!stored.contains(WG_KEY), "{stored}");
        assert!(!stored.contains(JWT), "{stored}");
        assert!(!stored.contains("overlay_wg_secret_key"), "{stored}");
        let line = &batch.lines[0];
        assert_eq!(line.msg, "overlay key [REDACTED_KEY] refused");
        let error = line.fields.get_str("error").expect("error");
        assert!(
            error.contains("TOML parse error at line 7, column 71\n"),
            "{error}"
        );
        assert!(error.contains("expected newline, `#`"), "{error}");
        let keys = line
            .fields
            .get_document("nested")
            .and_then(|n| n.get_array("keys"))
            .expect("nested keys");
        assert_eq!(keys[1].as_str(), Some("kept"));
        assert_eq!(line.fields.get_i64("n"), Ok(3));
    }

    /// The scrub is tight: ordinary lines, ids, hex digests, addresses and the
    /// word "Bearer" without a credential after it included, are stored exactly
    /// as they came.
    #[test]
    fn build_batch_leaves_ordinary_lines_alone() {
        let mut relay = sample_line("overlay relay: allocated (srflx 203.0.113.7:41641)");
        relay.target = "roomlerd::overlay::relay_link".to_string();
        relay.fields = bson::doc! {
            "peer": "6a1f0c2e9b3d4a5f6e7d8c9b",
            "own": "203.0.113.7:41641",
            "sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "why": "missing Bearer token",
            "ok": true,
        };
        let refused = sample_line("upload refused: missing Bearer token, retrying");
        let before = vec![relay.clone(), refused.clone()];
        let batch = build_batch(
            ObjectId::new(),
            None,
            None,
            sample_payload(LogSource::Agent, vec![relay, refused]),
        )
        .expect("build_batch");
        for (stored, sent) in batch.lines.iter().zip(&before) {
            assert_eq!(stored.msg, sent.msg);
            assert_eq!(stored.target, sent.target);
            assert_eq!(stored.fields, sent.fields);
        }
    }

    /// FR-22 part 4 — the RC viewer's connect-timing record, in exactly the
    /// shape `ui/src/composables/rcConnectTimingUpload.ts` emits. The browser
    /// has no other way to learn that its `ts` (canonical extended JSON —
    /// what `bson::DateTime` round-trips through `serde_json`) and its
    /// `fields` document parse on this route: a POST that 422s is swallowed
    /// by design, so a shape drift would show up as `agent_logs` staying
    /// empty — which is indistinguishable from nobody connecting.
    #[test]
    fn browser_connect_timing_record_parses() {
        let raw = r#"{
            "tenant_id": "69f0000000000000000000a1",
            "source": "browser",
            "session_id": "69f0000000000000000000b2",
            "lines": [{
                "ts": {"$date": {"$numberLong": "1758826800000"}},
                "level": "INFO",
                "target": "rc.connect",
                "msg": "attempt 1 ttff 2736ms — ws_ready:+12 turn_ready:+64 probes_ready:+28 request_sent:+3 session_created:+43 ready:+53 offer_sent:+9 answer:+214 pc_connected:+1150 dc_open:+788 first_frame:+372",
                "fields": {
                    "v": 1,
                    "outcome": "first_frame",
                    "attempt": 1,
                    "after_drop": false,
                    "hidden": false,
                    "stalled_at": null,
                    "ttff_ms": 2736,
                    "agent_id": "69f0000000000000000000c3",
                    "marks": {"ws_ready": 12, "turn_ready": 76, "first_frame": 2736}
                }
            }]
        }"#;
        let payload: BrowserLogBatchPayload =
            serde_json::from_str(raw).expect("the viewer's record must parse");
        assert_eq!(payload.inner.source, LogSource::Browser);
        assert_eq!(
            payload.inner.session_id.as_deref(),
            Some("69f0000000000000000000b2")
        );
        let tid = ObjectId::parse_str(&payload.tenant_id).expect("tenant hex");
        let batch =
            build_batch(tid, None, Some(ObjectId::new()), payload.inner).expect("build_batch");
        assert_eq!(batch.line_count, 1);
        let line = &batch.lines[0];
        assert_eq!(line.ts.timestamp_millis(), 1_758_826_800_000);
        assert_eq!(line.level, LogLevel::Info);
        assert_eq!(line.target, "rc.connect");
        assert_eq!(line.fields.get_str("outcome"), Ok("first_frame"));
        assert_eq!(line.fields.get_bool("hidden"), Ok(false));
        assert!(matches!(
            line.fields.get("stalled_at"),
            Some(bson::Bson::Null)
        ));
        let marks = line.fields.get_document("marks").expect("marks doc");
        // serde_json hands an integer to bson as i64 or i32 depending on the
        // path; the record only promises "a number of milliseconds".
        let first_frame = match marks.get("first_frame") {
            Some(bson::Bson::Int32(v)) => i64::from(*v),
            Some(bson::Bson::Int64(v)) => *v,
            other => panic!("first_frame should be an integer, got {other:?}"),
        };
        assert_eq!(first_frame, 2736);
        // An unreached mark is ABSENT, never zero: its absence is the finding.
        assert!(marks.get("answer").is_none());
    }

    /// The abandoned shape: no `session_id` (a `requesting` stall never got
    /// one), `WARN`, and `stalled_at` naming the wait that never completed.
    #[test]
    fn browser_connect_timing_abandoned_record_parses() {
        let raw = r#"{
            "tenant_id": "69f0000000000000000000a1",
            "source": "browser",
            "lines": [{
                "ts": {"$date": {"$numberLong": "1758826804000"}},
                "level": "WARN",
                "target": "rc.connect",
                "msg": "attempt 1 INCOMPLETE (stalled waiting for session_created) — ws_ready:+12 turn_ready:+64 probes_ready:+28 request_sent:+3 session_created:— ready:— offer_sent:— answer:— pc_connected:— dc_open:— first_frame:—",
                "fields": {
                    "v": 1,
                    "outcome": "abandoned",
                    "attempt": 1,
                    "after_drop": false,
                    "hidden": true,
                    "stalled_at": "session_created",
                    "ttff_ms": null,
                    "agent_id": "69f0000000000000000000c3",
                    "marks": {"ws_ready": 12, "turn_ready": 76, "probes_ready": 104, "request_sent": 107}
                }
            }]
        }"#;
        let payload: BrowserLogBatchPayload = serde_json::from_str(raw).expect("parse");
        assert_eq!(payload.inner.session_id, None);
        let tid = ObjectId::parse_str(&payload.tenant_id).expect("tenant hex");
        let batch =
            build_batch(tid, None, Some(ObjectId::new()), payload.inner).expect("build_batch");
        let line = &batch.lines[0];
        assert_eq!(line.level, LogLevel::Warn);
        assert_eq!(line.fields.get_str("stalled_at"), Ok("session_created"));
        assert_eq!(line.fields.get_bool("hidden"), Ok(true));
        assert!(matches!(line.fields.get("ttff_ms"), Some(bson::Bson::Null)));
    }

    /// FR-16 L4 — the bench viewer's input-to-photon record, in exactly the
    /// shape `buildL4Upload` in `ui/src/composables/rcBench.ts` emits (its
    /// spec builds the same body from the same numbers). Round trips are
    /// fractional milliseconds, so they arrive as doubles; a shape drift
    /// would 422, be swallowed, and look like nobody benchmarking.
    #[test]
    fn browser_l4_bench_record_parses() {
        let raw = r#"{
            "tenant_id": "69f0000000000000000000a1",
            "source": "browser",
            "session_id": "69f0000000000000000000b2",
            "lines": [{
                "ts": {"$date": {"$numberLong": "1760364000000"}},
                "level": "INFO",
                "target": "rc.l4",
                "msg": "L4 n=3 lost=1 p50=45.6ms p95=61.2ms max=61.2ms path=video",
                "fields": {
                    "v": 1,
                    "paths": ["video"],
                    "n": 3,
                    "lost": 1,
                    "p50_ms": 45.6,
                    "p95_ms": 61.2,
                    "min_ms": 38,
                    "max_ms": 61.2,
                    "rtt_ms": [45.6, 38, 61.2],
                    "agent_id": "69f0000000000000000000c3"
                }
            }]
        }"#;
        let payload: BrowserLogBatchPayload =
            serde_json::from_str(raw).expect("the bench viewer's record must parse");
        let tid = ObjectId::parse_str(&payload.tenant_id).expect("tenant hex");
        let batch =
            build_batch(tid, None, Some(ObjectId::new()), payload.inner).expect("build_batch");
        let line = &batch.lines[0];
        assert_eq!(line.target, "rc.l4");
        assert_eq!(line.level, LogLevel::Info);
        assert_eq!(line.fields.get_f64("p50_ms"), Ok(45.6));
        let rtts = line.fields.get_array("rtt_ms").expect("rtt_ms array");
        assert_eq!(rtts.len(), 3);
        assert_eq!(
            line.fields
                .get_array("paths")
                .expect("paths")
                .first()
                .and_then(|p| p.as_str()),
            Some("video")
        );
    }

    // The scrub's own rules, and what each leaves alone, are tested where
    // they live: `crate::diagnostics::tests`.

    // The bearer-parsing test moved with the behaviour, to
    // `crate::extractors::agent` — this route no longer parses the header
    // itself; the `AuthAgent` extractor does, and checks the agent row too.
}

// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! The one place an agent JWT turns into an authorization decision.
//!
//! ## Why this exists
//!
//! An agent token lives for a **year**. Verifying its signature proves only
//! that we minted it — not that the device it names is still one we accept.
//!
//! The `/ws?role=agent` upgrade has always loaded the row and refused a deleted
//! or quarantined agent. The HTTP ingest routes did not: `agent_log::ingest_agent`
//! and `agent_crash::ingest` verified the token, parsed the ids straight out of
//! the claims, and wrote. So **deleting or quarantining a device did not stop it
//! writing** — for up to a year, with no revocation short of rotating the JWT
//! secret for the entire fleet. Sessions here are stateless, so the row IS the
//! revocation list; a path that never reads it has no revocation at all.
//!
//! ## Why an extractor rather than a helper call
//!
//! A helper is a step a handler can forget, and the two that forgot it are the
//! evidence. As an extractor the check is not a step at all: the only way to
//! obtain an authenticated `agent_id`/`tenant_id` is to have passed it. A new
//! agent-authed route gets the check by writing its signature.
//!
//! The WS paths (`ws::handler`, `ws::derp`) cannot use an extractor — their
//! token arrives in a query parameter on an upgrade — so they share the
//! *decision* instead, via [`refusal_reason`]. That function is the single
//! definition of "this agent may still act"; nothing should re-spell it.

use axum::{
    extract::FromRequestParts,
    http::{header, request::Parts},
};
use bson::oid::ObjectId;
use roomler_ai_remote_control::models::{Agent, AgentStatus};

use crate::{error::ApiError, extractors::auth::FromRef, state::AppState};

/// An agent that authenticated with its own JWT **and** still has a row we
/// accept. Carries the row so a handler that needs it does not re-read.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AuthAgent {
    pub agent_id: ObjectId,
    pub tenant_id: ObjectId,
    pub agent: Agent,
}

/// The single definition of "this agent may still act". `Some(reason)` refuses.
///
/// Shared with the WS paths so there is exactly one answer to the question.
/// Deliberately matches what `/ws?role=agent` has always enforced — deleted or
/// quarantined — rather than quietly widening the policy. (`AgentStatus::Unenrolled`
/// is not included: it is only ever written to `overlay_nodes`, never to an
/// agent row. Widening this is a policy change and should be argued on its own.)
pub fn refusal_reason(agent: &Agent) -> Option<&'static str> {
    refusal_reason_parts(agent.deleted_at.is_some(), agent.status)
}

/// The rule itself, over just the two fields it reads.
///
/// Split out ONLY so the tests can drive it: `Agent` has no constructor, and a
/// struct literal in a test breaks every time the model gains a field — the
/// failure mode this repo has already been bitten by. Do not "simplify" this
/// back into one function; the seam is what keeps the rule tested.
fn refusal_reason_parts(deleted: bool, status: AgentStatus) -> Option<&'static str> {
    if deleted {
        // Deletion wins over status: the cascade tombstones the row without
        // necessarily rewriting `status`, so an Online-looking tombstone must
        // still refuse.
        Some("agent has been deleted")
    } else if matches!(status, AgentStatus::Quarantined) {
        Some("agent is quarantined")
    } else {
        None
    }
}

/// The bearer token from an `Authorization` header, or `None` if it is
/// missing, empty or a different scheme.
///
/// Agent tokens travel in this header only — never a cookie. An agent is not a
/// browser, and accepting a cookie here would widen the surface for no caller.
///
/// Moved here (with its tests) from the duplicate `extract_bearer` that both
/// `agent_log` and `agent_crash` carried. The scheme match is case-insensitive
/// per RFC 7235 §2.1, which is slightly wider than the `Bearer `/`bearer `
/// pair the originals accepted and strictly more correct.
fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, rest) = raw.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = rest.trim();
    (!token.is_empty()).then_some(token)
}

impl<S> FromRequestParts<S> for AuthAgent
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app_state = AppState::from_ref(state);

        let token = bearer_token(&parts.headers)
            .ok_or_else(|| ApiError::Unauthorized("Missing Authorization header".to_string()))?;

        // Audience-checked: `verify_agent_token` rejects a user JWT.
        let claims = app_state
            .auth
            .verify_agent_token(token)
            .map_err(|e| ApiError::Unauthorized(e.to_string()))?;

        let agent_id = ObjectId::parse_str(&claims.sub)
            .map_err(|_| ApiError::Unauthorized("invalid agent_id in claims".to_string()))?;
        let tenant_id = ObjectId::parse_str(&claims.tenant_id)
            .map_err(|_| ApiError::Unauthorized("invalid tenant_id in claims".to_string()))?;

        // The row is the revocation list. A lookup FAILURE is a 500, not a
        // 401: a Mongo blip must not tell a healthy fleet its credentials were
        // revoked, which would turn a database wobble into an enrollment storm.
        //
        // `NotFound` is the one exception, and FR-51 is what made it real: a
        // REAPED ephemeral row is hard-deleted, so "no row" is Mongo's
        // AUTHORITATIVE answer, not a wobble — pre-FR-51 a gone device always
        // still had a tombstone and took the 401 below. Mapping NotFound to
        // 500 would have a reaped-but-still-running device retrying forever
        // against what reads as server trouble, instead of hearing that its
        // credential is dead (and the agent's self-unenroll deliberately
        // treats 401 as "already gone").
        let agent = app_state
            .agents
            .find_in_tenant(tenant_id, agent_id)
            .await
            .map_err(|e| match e {
                roomler_ai_services::dao::base::DaoError::NotFound => {
                    tracing::info!(%agent_id, %tenant_id, "refusing agent-authed request: row gone");
                    ApiError::Unauthorized("no such device".to_string())
                }
                other => ApiError::Internal(format!("agent lookup: {other}")),
            })?;

        if let Some(reason) = refusal_reason(&agent) {
            tracing::info!(%agent_id, %tenant_id, reason, "refusing agent-authed request");
            // 401, not 403: the credential itself is no longer accepted, and
            // that is what the device needs to hear.
            return Err(ApiError::Unauthorized(reason.to_string()));
        }

        Ok(AuthAgent {
            agent_id,
            tenant_id,
            agent,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue, header::AUTHORIZATION};

    // ── bearer parsing (ported from the duplicate `extract_bearer` that
    //    `agent_log` and `agent_crash` each carried before this extractor) ──

    #[test]
    fn bearer_returns_the_token_after_the_scheme() {
        let mut h = HeaderMap::new();
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer abc.def.ghi"),
        );
        assert_eq!(bearer_token(&h), Some("abc.def.ghi"));
    }

    #[test]
    fn bearer_scheme_is_case_insensitive() {
        for raw in ["bearer xyz", "BEARER xyz", "BeArEr xyz"] {
            let mut h = HeaderMap::new();
            h.insert(AUTHORIZATION, HeaderValue::from_str(raw).unwrap());
            assert_eq!(bearer_token(&h), Some("xyz"), "{raw}");
        }
    }

    #[test]
    fn bearer_returns_none_on_missing_header() {
        assert_eq!(bearer_token(&HeaderMap::new()), None);
    }

    #[test]
    fn bearer_returns_none_on_empty_token() {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, HeaderValue::from_static("Bearer    "));
        assert_eq!(bearer_token(&h), None);
    }

    #[test]
    fn bearer_returns_none_on_a_different_scheme() {
        let mut h = HeaderMap::new();
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Basic Zm9vOmJhcg=="),
        );
        assert_eq!(bearer_token(&h), None);
    }

    // ── the revocation rule ──

    #[test]
    fn a_live_agent_is_allowed() {
        assert!(refusal_reason_parts(false, AgentStatus::Online).is_none());
        assert!(refusal_reason_parts(false, AgentStatus::Offline).is_none());
        // Never written to an agent row today, but it is not a refusal reason
        // and must not silently become one.
        assert!(refusal_reason_parts(false, AgentStatus::Unenrolled).is_none());
    }

    #[test]
    fn a_quarantined_agent_is_refused() {
        assert_eq!(
            refusal_reason_parts(false, AgentStatus::Quarantined),
            Some("agent is quarantined")
        );
    }

    #[test]
    fn a_deleted_agent_is_refused_whatever_its_status_says() {
        for status in [
            AgentStatus::Online,
            AgentStatus::Offline,
            AgentStatus::Quarantined,
            AgentStatus::Unenrolled,
        ] {
            assert_eq!(
                refusal_reason_parts(true, status),
                Some("agent has been deleted"),
                "a tombstoned row must refuse regardless of {status:?}"
            );
        }
    }
}

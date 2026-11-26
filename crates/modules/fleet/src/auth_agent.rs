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
use roomler_ai_remote_control::models::{Agent, AgentStatus, TokenRotation};

use axum::extract::FromRef;

use roomler_core::ApiError;

use crate::FleetState;

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

/// FR-103: the single definition of "this token is not of the row's
/// generation". `Some(reason)` refuses. A removal bumps the row's `token_gen`
/// (`AgentDao::soft_delete`, `TunnelClientDao::soft_delete`), and so does an
/// agent's token rotation (FR-155, `AgentDao::rotate_token`). Enrollment,
/// renewal and rotation mint at the current value, so a token whose `gen` is
/// not the row's was minted before a removal or a rotation. Shared with the
/// socket, DERP and the tunnel-client paths, as [`refusal_reason`] is.
///
/// Every row never removed or rotated is at 0, and so is every pre-FR-103
/// token (no `gen` claim), so before either this refuses nothing. `enforced`
/// is `rc.token_generation_check`, the kill switch.
///
/// FR-155 Q2: `rotated` says the row's generation came from a rotation
/// ([`generation_from_rotation`]). A token below it is then refused in the
/// rotation's words, so neither the device nor a reader of the log looks for
/// a removal that never happened. A tunnel client does not rotate and passes
/// `false`.
///
/// Callers check [`refusal_reason`] FIRST, so a removed device hears that it
/// was deleted (its typed goodbye) rather than this.
pub fn generation_refusal(
    claims_gen: u32,
    row_gen: u32,
    rotated: bool,
    enforced: bool,
) -> Option<&'static str> {
    if !enforced || claims_gen == row_gen {
        return None;
    }
    Some(if rotated && claims_gen < row_gen {
        "this credential was issued before the device's token rotation; use the token it issued"
    } else {
        "this credential was issued before the device was removed; re-enroll it"
    })
}

/// FR-155: whether the row's generation came from a token rotation, i.e. its
/// last rotation made it. A removal after a rotation advances the generation
/// past the one the rotation made. What the agent callers pass to
/// [`generation_refusal`].
pub fn generation_from_rotation(agent: &Agent) -> bool {
    rotation_made_generation(agent.token_gen, agent.token_rotation.as_ref())
}

/// The rule over the two fields it reads, split out for the tests the same
/// way [`refusal_reason_parts`] is.
fn rotation_made_generation(row_gen: u32, last_rotation: Option<&TokenRotation>) -> bool {
    last_rotation.is_some_and(|r| r.generation == row_gen)
}

/// FR-155: who calls `POST /api/agent/self/rotate`, read from the token alone
/// ([`authenticate_for_rotation`]).
#[derive(Debug, Clone, Copy)]
pub struct RotationCaller {
    pub agent_id: ObjectId,
    pub tenant_id: ObjectId,
    /// The token's `gen` claim.
    pub generation: u32,
    /// The token's `exp`, unix seconds.
    pub exp: i64,
}

/// FR-155: the rotation route's own admission path, beside
/// [`authenticate_for_renewal`]. This half reads the token alone: a bearer
/// of the agent audience with our signature, and `exp` enforced as on every
/// route but renewal (an expired token renews first). It does NOT compare
/// the generation: a call asking again for the answer to a rotation presents
/// a token one below the row's, which [`generation_refusal`] refuses
/// everywhere else. The route then reads the row (tombstone included, so a
/// removed device is told so), applies [`refusal_reason`] and then
/// [`rotation_admission`], and audits each decision.
pub fn authenticate_for_rotation(
    fleet: &FleetState,
    headers: &axum::http::HeaderMap,
) -> Result<RotationCaller, ApiError> {
    let token = bearer_token(headers)
        .ok_or_else(|| ApiError::Unauthorized("Missing Authorization header".to_string()))?;
    let claims = fleet
        .auth
        .verify_agent_token(token)
        .map_err(|e| ApiError::Unauthorized(e.to_string()))?;
    let agent_id = ObjectId::parse_str(&claims.sub)
        .map_err(|_| ApiError::Unauthorized("invalid agent_id in claims".to_string()))?;
    let tenant_id = ObjectId::parse_str(&claims.tenant_id)
        .map_err(|_| ApiError::Unauthorized("invalid tenant_id in claims".to_string()))?;
    Ok(RotationCaller {
        agent_id,
        tenant_id,
        generation: claims.generation,
        exp: claims.exp,
    })
}

/// FR-155: what [`rotation_admission`] decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationAdmission {
    /// The token is of the row's generation: a first call, which the
    /// conditional update decides (`AgentDao::rotate_token`).
    First,
    /// The answer to the row's last rotation, asked again with its rotation
    /// key: minted at the row's generation, and nothing else changes.
    Replay,
    Refused(RotationRefusal),
}

/// FR-155: why the rotation route refuses a token of a row it accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationRefusal {
    /// The token's `exp` has passed: it renews first.
    Expired,
    /// The token is neither of the row's generation nor of the one the row's
    /// last rotation replaced.
    Generation,
    /// The token is of the generation the last rotation replaced, but the
    /// rotation key is not that rotation's.
    RotationKey,
}

impl RotationRefusal {
    /// The audit row's reason.
    pub fn code(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::Generation => "generation",
            Self::RotationKey => "rotation_key",
        }
    }

    /// What the device is told, with a 401.
    pub fn message(self) -> &'static str {
        match self {
            Self::Expired => "this token has expired; renew it, then rotate",
            Self::Generation => "this token is not of a generation this device can rotate from",
            Self::RotationKey => "the rotation key is not the one of the device's last rotation",
        }
    }
}

/// FR-155: the rotation route's admission, over the token's generation and
/// `exp`, the row [`refusal_reason`] accepted (its generation and its last
/// rotation), and the sha256 of the rotation key the call sent. `exp` first,
/// with no extension. Then exactly one of:
/// - the token's generation is the row's: a first call;
/// - it is one below the row's, the row's last rotation made the row's
///   generation, and the key hashes to that rotation's: the answer to that
///   rotation, asked again.
///
/// Nothing else is admitted.
pub fn rotation_admission(
    token_gen: u32,
    token_exp: i64,
    now: i64,
    row_gen: u32,
    last_rotation: Option<&TokenRotation>,
    key_sha256: &str,
) -> RotationAdmission {
    if token_exp <= now {
        return RotationAdmission::Refused(RotationRefusal::Expired);
    }
    if token_gen == row_gen {
        return RotationAdmission::First;
    }
    match last_rotation {
        Some(last)
            if row_gen.checked_sub(1) == Some(token_gen)
                && rotation_made_generation(row_gen, last_rotation) =>
        {
            if same_bytes(last.key_sha256.as_bytes(), key_sha256.as_bytes()) {
                RotationAdmission::Replay
            } else {
                RotationAdmission::Refused(RotationRefusal::RotationKey)
            }
        }
        _ => RotationAdmission::Refused(RotationRefusal::Generation),
    }
}

/// Equal, in time that does not depend on where two inputs of one length
/// differ.
fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
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

/// The decision itself, over the request headers — what the extractor below
/// runs, callable by a router whose state cannot `FromRef` a `FleetState`.
///
/// FR-84 D5a split this out of the extractor for the HOST's `/api/agent/self/*`
/// routes: their state is `AppState`, on which the fleet module is an
/// `Option` (a switched-off module is unmounted, never a boot refusal), so
/// the `FleetState: FromRef<S>` bound above can't be met there. The host
/// wraps this in its own extractor and answers 503 for the `None`; the rules
/// are these, once: bearer only, audience-checked, status-checked on every
/// use, deletion wins over status, a lookup FAILURE is 500 not 401.
pub async fn authenticate(
    fleet: &FleetState,
    headers: &axum::http::HeaderMap,
) -> Result<AuthAgent, ApiError> {
    let token = bearer_token(headers)
        .ok_or_else(|| ApiError::Unauthorized("Missing Authorization header".to_string()))?;

    // Audience-checked: `verify_agent_token` rejects a user JWT, and FR-94's
    // service credential.
    let claims = fleet
        .auth
        .verify_agent_token(token)
        .map_err(|e| ApiError::Unauthorized(e.to_string()))?;

    let (agent_id, tenant_id, agent) = accepted_agent(
        fleet,
        &claims.sub,
        &claims.tenant_id,
        Some(claims.generation),
    )
    .await?;
    Ok(AuthAgent {
        agent_id,
        tenant_id,
        agent,
    })
}

/// FR-103: [`authenticate`] for `POST /api/agent/self/renew` ONLY. The token
/// may be up to `RENEWAL_GRACE_SECS` past its `exp`
/// (`AuthService::verify_agent_token_for_renewal`), so a device that slept
/// through its anniversary can buy a fresh one. Everything else is the same
/// decision: audience, the row (deleted or quarantined refuses), and the
/// generation. An expired token therefore renews only for a device that is
/// still one we accept.
pub async fn authenticate_for_renewal(
    fleet: &FleetState,
    headers: &axum::http::HeaderMap,
) -> Result<AuthAgent, ApiError> {
    let token = bearer_token(headers)
        .ok_or_else(|| ApiError::Unauthorized("Missing Authorization header".to_string()))?;
    let claims = fleet
        .auth
        .verify_agent_token_for_renewal(token)
        .map_err(|e| ApiError::Unauthorized(e.to_string()))?;
    let (agent_id, tenant_id, agent) = accepted_agent(
        fleet,
        &claims.sub,
        &claims.tenant_id,
        Some(claims.generation),
    )
    .await?;
    Ok(AuthAgent {
        agent_id,
        tenant_id,
        agent,
    })
}

/// FR-94 — a Windows device's SYSTEM service, authenticated with its OWN
/// credential (`TokenType::AgentService`, minted by `POST /api/agent/anchor`)
/// and still a device we accept. The agent token never passes here, and this
/// credential never passes [`AuthAgent`]. On an attended install the agent
/// token sits in the signed-in user's writable config, and nothing that user
/// can write may decide what SYSTEM runs (FR-94 §4).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AuthAgentService {
    pub agent_id: ObjectId,
    pub tenant_id: ObjectId,
    pub agent: Agent,
}

/// [`authenticate`]'s twin for the service credential: the same bearer rule,
/// the same row check, a different audience, and one more rule: the
/// credential must name the device's CURRENT anchor.
pub async fn authenticate_service(
    fleet: &FleetState,
    headers: &axum::http::HeaderMap,
) -> Result<AuthAgentService, ApiError> {
    let token = bearer_token(headers)
        .ok_or_else(|| ApiError::Unauthorized("Missing Authorization header".to_string()))?;
    let claims = fleet
        .auth
        .verify_agent_service_token(token)
        .map_err(|e| ApiError::Unauthorized(e.to_string()))?;
    // No generation: a removal ends this credential through its anchor
    // (#2133, `soft_delete` clears `exec_anchor`), checked just below.
    let (agent_id, tenant_id, agent) =
        accepted_agent(fleet, &claims.sub, &claims.tenant_id, None).await?;
    if !anchor_is_current(&agent, &claims.anchor_id) {
        tracing::info!(%agent_id, %tenant_id, "refusing service-authed request: anchor superseded");
        // 401: this credential is dead for good, and the service must hear
        // "re-anchor", not "try again".
        return Err(ApiError::Unauthorized(
            "this device's SYSTEM service was re-anchored or un-anchored; anchor it again".into(),
        ));
    }
    Ok(AuthAgentService {
        agent_id,
        tenant_id,
        agent,
    })
}

/// A service credential is live only while it names the device's current
/// anchor. Re-anchoring writes a new id, so every credential an earlier
/// anchor minted dies with it: one live SYSTEM service per device, and an
/// administrator can revoke one without removing the device. An empty id
/// (a row from before the field) matches nothing.
fn anchor_is_current(agent: &Agent, anchor_id: &str) -> bool {
    anchor_matches(
        agent.exec_anchor.as_ref().map(|a| a.anchor_id.as_str()),
        anchor_id,
    )
}

/// The rule over just the two ids, split out for the tests the same way
/// [`refusal_reason_parts`] is.
fn anchor_matches(current: Option<&str>, presented: &str) -> bool {
    current.is_some_and(|c| !c.is_empty() && c == presented)
}

/// The row check both credentials share: the ids in the claims, the row
/// they name, [`refusal_reason`], and for an agent token (`gen` is `Some`)
/// [`generation_refusal`]. One definition of "this device may still act",
/// whichever credential is asking.
async fn accepted_agent(
    fleet: &FleetState,
    sub: &str,
    tenant: &str,
    generation: Option<u32>,
) -> Result<(ObjectId, ObjectId, Agent), ApiError> {
    let agent_id = ObjectId::parse_str(sub)
        .map_err(|_| ApiError::Unauthorized("invalid agent_id in claims".to_string()))?;
    let tenant_id = ObjectId::parse_str(tenant)
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
    //
    // ANY, not live (#1821): this reader's job IS the tombstone. A removed
    // device must take the `refusal_reason` branch below and hear "deleted",
    // and the `NotFound` arm must keep meaning what the paragraph above says
    // — a REAPED row — so the log can tell the two apart.
    let agent = fleet
        .agents
        .find_any_in_tenant(tenant_id, agent_id)
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

    // FR-103 — after the row check, so a removed device hears "deleted".
    if let Some(reason) = generation.and_then(|g| {
        generation_refusal(
            g,
            agent.token_gen,
            generation_from_rotation(&agent),
            fleet.settings.rc.token_generation_check,
        )
    }) {
        tracing::info!(
            %agent_id, %tenant_id, reason, row_gen = agent.token_gen,
            "refusing agent-authed request: token of another generation"
        );
        return Err(ApiError::Unauthorized(reason.to_string()));
    }

    Ok((agent_id, tenant_id, agent))
}

impl<S> FromRequestParts<S> for AuthAgentService
where
    FleetState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let fleet = FleetState::from_ref(state);
        authenticate_service(&fleet, &parts.headers).await
    }
}

impl<S> FromRequestParts<S> for AuthAgent
where
    FleetState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app_state = FleetState::from_ref(state);
        authenticate(&app_state, &parts.headers).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue, header::AUTHORIZATION};
    use roomler_ai_remote_control::models::TokenRotation;

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

    /// FR-94 — a service credential is live only for the device's current
    /// anchor. No anchor, another anchor, and an empty id on both sides
    /// (a row from before the field) all refuse.
    #[test]
    fn only_the_current_anchor_is_accepted() {
        assert!(anchor_matches(Some("a1"), "a1"));
        assert!(!anchor_matches(Some("a2"), "a1"), "re-anchored");
        assert!(!anchor_matches(None, "a1"), "un-anchored");
        assert!(!anchor_matches(Some(""), ""), "an empty id matches nothing");
    }

    /// FR-103: a token from before the device's removal is refused. Before
    /// any removal everything is at 0 and passes, a pre-FR-103 token
    /// included. The kill switch passes everything.
    #[test]
    fn only_a_token_of_the_rows_generation_is_accepted() {
        assert_eq!(generation_refusal(0, 0, false, true), None, "never removed");
        assert_eq!(
            generation_refusal(1, 1, false, true),
            None,
            "minted after it"
        );
        assert!(
            generation_refusal(0, 1, false, true).is_some(),
            "minted before the removal"
        );
        assert!(
            generation_refusal(2, 1, false, true).is_some(),
            "a generation the row never had"
        );
        assert_eq!(
            generation_refusal(0, 1, false, false),
            None,
            "the kill switch"
        );
    }

    /// FR-155 (T11, Q2): a token below a generation a rotation made is
    /// refused in the rotation's words, and one below a generation a removal
    /// made in the removal's. A token of the row's generation is not refused,
    /// and with the switch off nothing is.
    #[test]
    fn a_rotated_generation_is_refused_with_its_own_reason() {
        let rotated = generation_refusal(2, 3, true, true).expect("below a rotation");
        let removed = generation_refusal(2, 3, false, true).expect("below a removal");
        assert_ne!(rotated, removed, "each cause has its own words");
        assert!(rotated.contains("rotation"), "{rotated}");
        assert!(removed.contains("removed"), "{removed}");
        assert_eq!(
            generation_refusal(0, 3, true, true),
            Some(rotated),
            "any generation below the one a rotation made"
        );
        assert_eq!(
            generation_refusal(4, 3, true, true),
            Some(removed),
            "a generation the row never had, as before"
        );
        assert_eq!(generation_refusal(3, 3, true, true), None, "equal");
        assert_eq!(generation_refusal(3, 3, false, true), None, "equal");
        assert_eq!(generation_refusal(2, 3, true, false), None, "switch off");
        assert_eq!(generation_refusal(2, 3, false, false), None, "switch off");

        // What the callers pass: the row's generation came from a rotation
        // when its last rotation made it.
        let made = |generation| rotation_at(generation, "aa");
        assert!(rotation_made_generation(3, Some(&made(3))));
        assert!(
            !rotation_made_generation(3, Some(&made(2))),
            "a removal came after the rotation"
        );
        assert!(!rotation_made_generation(3, None), "never rotated");
    }

    fn rotation_at(generation: u32, key_sha256: &str) -> TokenRotation {
        TokenRotation {
            generation,
            key_sha256: key_sha256.to_string(),
            at: bson::DateTime::from_millis(0),
        }
    }

    /// FR-155 (T9): the rotation route's admission. A token of the row's
    /// generation is a first call. A token one generation below, when the
    /// row's last rotation made the row's generation and the rotation key
    /// hashes to that rotation's, asks again for its answer. Anything else is
    /// refused, and so is a token whose `exp` has passed, with no extension.
    #[test]
    fn rotation_admission() {
        use RotationAdmission::{First, Refused, Replay};
        let key = "ab".repeat(32);
        let other = "cd".repeat(32);
        let now = 1_800_000_000;
        let live = now + 3600;
        let made = |generation| rotation_at(generation, &key);
        let admit = |token_gen, exp, row_gen, last: Option<&TokenRotation>, hash: &str| {
            super::rotation_admission(token_gen, exp, now, row_gen, last, hash)
        };

        // A first call.
        assert_eq!(admit(0, live, 0, None, &key), First, "never rotated");
        assert_eq!(
            admit(3, live, 3, Some(&made(3)), &other),
            First,
            "with the token a rotation answered, any key"
        );
        assert_eq!(
            admit(3, live, 3, Some(&made(2)), &key),
            First,
            "after a removal that followed a rotation"
        );

        // An answer asked again.
        assert_eq!(admit(2, live, 3, Some(&made(3)), &key), Replay);
        assert_eq!(
            admit(2, live, 3, Some(&made(3)), &other),
            Refused(RotationRefusal::RotationKey),
            "another key"
        );
        assert_eq!(
            admit(2, live, 3, None, &key),
            Refused(RotationRefusal::Generation),
            "the row's generation came from a removal"
        );
        assert_eq!(
            admit(2, live, 3, Some(&made(2)), &key),
            Refused(RotationRefusal::Generation),
            "the last rotation made an earlier generation"
        );

        // Neither.
        assert_eq!(
            admit(1, live, 3, Some(&made(3)), &key),
            Refused(RotationRefusal::Generation),
            "two below"
        );
        assert_eq!(
            admit(4, live, 3, Some(&made(3)), &key),
            Refused(RotationRefusal::Generation),
            "above the row's"
        );

        // `exp`, for a first call and an answer asked again alike.
        assert_eq!(
            admit(3, now, 3, None, &key),
            Refused(RotationRefusal::Expired),
            "at its exp"
        );
        assert_eq!(
            admit(2, now - 1, 3, Some(&made(3)), &key),
            Refused(RotationRefusal::Expired),
            "past it"
        );
        assert_eq!(admit(3, now + 1, 3, None, &key), First, "a second left");
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

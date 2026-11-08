// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! The one owner of a single-use enrollment token's redemption (#3328).
//!
//! An enrollment token is worth one long-lived credential. That used to be a
//! convention each enrollment door had to remember: the daemon door claimed
//! nothing until the 2026-08-05 field finding, and the tunnel door claimed
//! nothing at all. It is a type now:
//!
//! - Verifying a single-use audience (`Enrollment`, `TunnelEnrollment`) yields
//!   an [`Unspent`] token. A door reads the claims from it for the refusals
//!   that must leave the token usable (an archived org, the plan's cap, a
//!   re-enrollment the token's subject may not make, a read that failed), and
//!   can mint nothing from it.
//! - [`UsedTokenDao::spend`](crate::dao::used_token::UsedTokenDao::spend)
//!   records the one use on the ledger and turns it into a [`Spent`] token, or
//!   answers the [`SpendRefusal`] that says why not.
//! - The mints that turn an enrollment into a long-lived credential
//!   (`AuthService::issue_agent_token`, `issue_tunnel_client_token` and
//!   `issue_agent_service_token`) take a [`Spent`] token by value, and nothing
//!   outside this crate can make one. A door that skips the spend does not
//!   compile, and one spend mints one credential.
//! - An FR-51 ephemeral key is reusable by design: its use is claimed on its
//!   own row ([`ClaimedKeyUse`], `EnrollmentKeyDao::claim_use`), and the agent
//!   mint takes that instead.
//!
//! Renewal is not enrollment. A renewal authenticates with a credential the
//! device already holds, and keeps its own entry (`AuthService::renew_*`).

use bson::oid::ObjectId;
use roomler_ai_remote_control::models::EnrollmentKey;

use super::{AuthError, EnrollmentClaims, TunnelEnrollmentClaims};

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::EnrollmentClaims {}
    impl Sealed for super::TunnelEnrollmentClaims {}
}

/// The claims of a single-use enrollment token. Sealed: the two single-use
/// audiences are the only ones, and a new kind of enrollment token joins them
/// here, where the spend is.
pub trait SingleUse: sealed::Sealed {
    /// The token's unique id, which the ledger keys its one use on.
    fn jti(&self) -> &str;
    /// The org the token was minted for, hex.
    fn tenant_id(&self) -> &str;
    /// The person the token was minted for, hex.
    fn subject(&self) -> &str;
}

impl SingleUse for EnrollmentClaims {
    fn jti(&self) -> &str {
        &self.jti
    }
    fn tenant_id(&self) -> &str {
        &self.tenant_id
    }
    fn subject(&self) -> &str {
        &self.sub
    }
}

impl SingleUse for TunnelEnrollmentClaims {
    fn jti(&self) -> &str {
        &self.jti
    }
    fn tenant_id(&self) -> &str {
        &self.tenant_id
    }
    fn subject(&self) -> &str {
        &self.sub
    }
}

/// A verified single-use enrollment token that has not been spent: its
/// signature, issuer, audience and expiry hold, and nothing yet says its one
/// use is taken. A door reads its claims and refuses whatever must leave it
/// usable, then spends it before it writes anything.
#[derive(Debug)]
#[must_use = "an enrollment door spends the token (`UsedTokenDao::spend`) before it writes anything"]
pub struct Unspent<C> {
    claims: C,
}

impl<C: SingleUse> Unspent<C> {
    pub(crate) fn new(claims: C) -> Self {
        Self { claims }
    }

    pub fn claims(&self) -> &C {
        &self.claims
    }

    pub(crate) fn into_claims(self) -> C {
        self.claims
    }
}

/// A single-use enrollment token whose one use is on the ledger. Only
/// [`UsedTokenDao`](crate::dao::used_token::UsedTokenDao) makes one, and the
/// mint it is handed to consumes it.
#[derive(Debug)]
pub struct Spent<C> {
    claims: C,
}

impl<C: SingleUse> Spent<C> {
    pub(crate) fn new(claims: C) -> Self {
        Self { claims }
    }

    pub fn claims(&self) -> &C {
        &self.claims
    }

    /// The org the token was minted for, the one the credential is for.
    pub(crate) fn tenant(&self) -> Result<ObjectId, AuthError> {
        ObjectId::parse_str(self.claims.tenant_id())
            .map_err(|_| AuthError::InvalidToken("Invalid tenant_id claim".to_string()))
    }

    /// The person the token was minted for.
    pub(crate) fn subject(&self) -> Result<ObjectId, AuthError> {
        ObjectId::parse_str(self.claims.subject())
            .map_err(|_| AuthError::InvalidToken("Invalid subject claim".to_string()))
    }
}

/// What the ledger records a use for. The words are the ledger's `purpose`
/// field; the two agent ones predate this type and must not change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpendPurpose {
    /// `POST /api/agent/enroll`.
    AgentEnroll,
    /// `POST /api/agent/anchor` (FR-94, FR-104).
    AgentAnchor,
    /// `POST /api/tunnel-client/enroll`.
    TunnelEnroll,
}

impl SpendPurpose {
    pub fn as_str(self) -> &'static str {
        match self {
            SpendPurpose::AgentEnroll => "agent-enroll",
            SpendPurpose::AgentAnchor => "agent-anchor",
            SpendPurpose::TunnelEnroll => "tunnel-enroll",
        }
    }
}

/// Why a spend did not happen. Every door keeps the two apart, because they
/// send a person to different places: "already used" means someone used this
/// token before (401), and a ledger that could not answer means retry, the
/// token is still valid (503). Answering both with "already used" would send
/// an operator hunting a replay that is not there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpendRefusal {
    /// The ledger already holds this use.
    AlreadyUsed,
    /// The ledger could not answer, so nothing proves the token is unspent.
    LedgerUnavailable,
}

/// One use of an FR-51 ephemeral enrollment key, claimed on its row by
/// `EnrollmentKeyDao::claim_use`: the key's redemption, as a spend is a
/// single-use token's. Only that claim makes one.
#[derive(Debug)]
pub struct ClaimedKeyUse {
    key: EnrollmentKey,
}

impl ClaimedKeyUse {
    pub(crate) fn new(key: EnrollmentKey) -> Self {
        Self { key }
    }

    /// The key's row after the claim: its use counter already includes this one.
    pub fn key(&self) -> &EnrollmentKey {
        &self.key
    }
}

/// What an agent token may be minted from at enrollment: a single-use
/// enrollment token spent on the ledger, or one claimed use of an ephemeral
/// key.
#[derive(Debug)]
pub enum AgentEnrollment {
    Token(Spent<EnrollmentClaims>),
    EphemeralKey(ClaimedKeyUse),
}

impl AgentEnrollment {
    /// The org the credential is for: the token's, or the key's.
    pub(crate) fn tenant(&self) -> Result<ObjectId, AuthError> {
        match self {
            AgentEnrollment::Token(spent) => spent.tenant(),
            AgentEnrollment::EphemeralKey(used) => Ok(used.key.tenant_id),
        }
    }
}

/// What the daemon's enroll route accepts, told apart by AUDIENCE, never by
/// anything in the request body: a single-use enrollment token, unspent, or
/// the JWT half of an FR-51 ephemeral key, whose row decides each use.
#[derive(Debug)]
pub enum EnrollmentCredential {
    SingleUse(Unspent<EnrollmentClaims>),
    EphemeralKey(EnrollmentClaims),
}

impl EnrollmentCredential {
    pub fn claims(&self) -> &EnrollmentClaims {
        match self {
            EnrollmentCredential::SingleUse(token) => token.claims(),
            EnrollmentCredential::EphemeralKey(claims) => claims,
        }
    }
}

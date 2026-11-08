// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use bson::{DateTime, doc};
use mongodb::Database;

use crate::auth::{EnrollmentClaims, SingleUse, SpendPurpose, SpendRefusal, Spent, Unspent};

/// Single-use ledger for enrollment tokens.
///
/// Enrollment tokens were designed single-use — the mint even returns the
/// `jti` "so the caller may persist it for single-use checks" — but nothing
/// ever did, so a token stayed replayable for its whole 10-minute TTL. Field
/// 2026-08-05 confirmed it: an enrollment rejected by the device-cap was
/// accepted on a later retry with the SAME token.
///
/// That matters more now than it did: the cross-org "add device" flow mints a
/// token per click and pushes it over the wire (and, cross-pod, over the
/// Redis ctrl lane), so a captured token must be worth exactly one use.
///
/// `_id` IS the jti, which makes the claim a single insert: the unique
/// primary key does the arbitration, with no read-then-write race. Rows TTL
/// out an hour after use — comfortably past the 10-minute token lifetime, so
/// a replay always finds the record while the ledger stays small.
///
/// #3328 — the ledger is reached through [`Self::spend`] only, which takes an
/// [`Unspent`] token and hands back the [`Spent`] one the credential mints
/// require (`crate::auth::single_use`). Every enrollment door spends the same
/// way, and answers the same two refusals.
pub struct UsedTokenDao {
    collection: mongodb::Collection<bson::Document>,
}

impl UsedTokenDao {
    pub const COLLECTION: &'static str = "used_tokens";

    pub fn new(db: &Database) -> Self {
        Self {
            collection: db.collection(Self::COLLECTION),
        }
    }

    /// Spend a single-use enrollment token: record its one use for `purpose`,
    /// and hand back the [`Spent`] token a credential mint takes.
    ///
    /// A door calls this after every refusal that must leave the token usable
    /// (an archived org, the plan's cap, a re-enrollment the token's subject
    /// may not make, a read that failed) and before it writes anything.
    /// [`SpendRefusal::AlreadyUsed`] = someone used this token before;
    /// [`SpendRefusal::LedgerUnavailable`] = nothing proves it unspent, and
    /// the token is still valid for a retry.
    pub async fn spend<C: SingleUse>(
        &self,
        token: Unspent<C>,
        purpose: SpendPurpose,
    ) -> Result<Spent<C>, SpendRefusal> {
        self.claim(token.claims().jti(), purpose).await?;
        Ok(Spent::new(token.into_claims()))
    }

    /// FR-104 — the anchor's SECOND use of the token that enrolled the device
    /// it anchors: a token minted with `anchor: true` that enrolled this very
    /// device. Its own ledger key, `<jti>:anchor`, so it happens once, apart
    /// from the enrollment's use. Whether the token qualifies is the anchor
    /// route's decision, made against the device row; every other anchor
    /// spends the token's one use through [`Self::spend`].
    pub async fn spend_anchor_second_use(
        &self,
        token: Unspent<EnrollmentClaims>,
    ) -> Result<Spent<EnrollmentClaims>, SpendRefusal> {
        let key = format!("{}:anchor", token.claims().jti);
        self.claim(&key, SpendPurpose::AgentAnchor).await?;
        Ok(Spent::new(token.into_claims()))
    }

    /// Claim the ledger key `key` for `purpose`. `Ok(())` = this caller won it
    /// (first use); [`SpendRefusal::AlreadyUsed`] = it was already spent;
    /// [`SpendRefusal::LedgerUnavailable`] = the ledger could not answer,
    /// which is ALSO a refusal.
    ///
    /// ## Why this fails CLOSED
    ///
    /// It used to fail open, reasoning that "enrollment is how a fleet recovers,
    /// and bricking every enroll because the ledger is unreachable trades a
    /// narrow replay window for a total outage".
    ///
    /// That trade does not exist. Every enrollment door writes its row through
    /// `?` immediately after this claim (the daemon's `enroll_agent`, the
    /// tunnel door's `enroll_tunnel_client`, the anchor), and read the tenant
    /// just before it. **An enrollment cannot succeed without Mongo**, so there
    /// is no world in which failing open here rescues one. What failing open
    /// actually did was narrower and worse: in the one case where the
    /// `used_tokens` insert errors while the `agents` write succeeds — a
    /// transient blip, a primary stepdown — it let the replay through. It
    /// disabled the control precisely in the window where the control mattered.
    ///
    /// Failing closed costs a RETRY, not an enrollment: the token is still
    /// valid for the rest of its 10 minutes.
    ///
    /// ⚠️ One case does burn a token: an insert that SUCCEEDED but reported an
    /// error, after which the retry legitimately sees a duplicate key. That is
    /// why the two failures are different variants and the route reports them
    /// differently — an operator has to be able to tell "someone replayed this"
    /// from "mint a new one", and answering both with "already used" would send
    /// them hunting an attacker who is not there.
    async fn claim(&self, key: &str, purpose: SpendPurpose) -> Result<(), SpendRefusal> {
        let purpose = purpose.as_str();
        let doc = doc! {
            "_id": key,
            "purpose": purpose,
            "used_at": DateTime::now(),
        };
        match self.collection.insert_one(doc).await {
            Ok(_) => Ok(()),
            Err(e) => {
                if is_duplicate_key(&e) {
                    Err(SpendRefusal::AlreadyUsed)
                } else {
                    tracing::error!(
                        %purpose, %e,
                        "single-use token ledger unavailable; REFUSING the enrollment \
                         (cannot prove this token is unspent)"
                    );
                    Err(SpendRefusal::LedgerUnavailable)
                }
            }
        }
    }
}

fn is_duplicate_key(e: &mongodb::error::Error) -> bool {
    if let mongodb::error::ErrorKind::Write(mongodb::error::WriteFailure::WriteError(
        ref write_error,
    )) = *e.kind
    {
        return write_error.code == 11000;
    }
    false
}

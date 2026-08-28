// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use bson::{DateTime, doc};
use mongodb::Database;

use super::base::{DaoError, DaoResult};

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

    /// Claim `jti` for `purpose`. `Ok(())` = this caller won it (first use);
    /// `Err(DaoError::Validation)` = it was already spent; `Err(DaoError::Mongo)`
    /// = the ledger could not answer, which is ALSO a refusal.
    ///
    /// ## Why this fails CLOSED
    ///
    /// It used to fail open, reasoning that "enrollment is how a fleet recovers,
    /// and bricking every enroll because the ledger is unreachable trades a
    /// narrow replay window for a total outage".
    ///
    /// That trade does not exist. The caller writes the agent row through `?`
    /// immediately after this claim (`routes::remote_control::enroll_agent` →
    /// `agents.create` / `rehydrate`), and the device-cap check read the tenant
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
    pub async fn claim(&self, jti: &str, purpose: &str) -> DaoResult<()> {
        let doc = doc! {
            "_id": jti,
            "purpose": purpose,
            "used_at": DateTime::now(),
        };
        match self.collection.insert_one(doc).await {
            Ok(_) => Ok(()),
            Err(e) => {
                if is_duplicate_key(&e) {
                    Err(DaoError::Validation(
                        "this enrollment token has already been used".to_string(),
                    ))
                } else {
                    tracing::error!(
                        %purpose, %e,
                        "single-use token ledger unavailable; REFUSING the enrollment \
                         (cannot prove this token is unspent)"
                    );
                    Err(DaoError::Mongo(e))
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

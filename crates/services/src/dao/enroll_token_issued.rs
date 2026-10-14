// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-102 P3 — `enroll_token_issued`: one row per standard enrollment token
//! MINTED, keyed by its `jti`, updated when that jti is redeemed (spec §3e
//! item 1). Owned by the `fleet` module (§3o), which carries its index set.
//!
//! The row holds the token's **id**, never the token: `{_id: jti, tenant_id,
//! issued_by, issued_at, kind, redeemed_at, agent_id}`. The digest reads it
//! for "tokens issued → redeemed"; nothing else does. FR-51's ephemeral keys
//! already have `enrollment_keys` + `enrollment_key_uses` and are not
//! duplicated here.
//!
//! Both writes are best-effort at their call sites: a mint that could not
//! record itself still hands the admin a token, and an enroll that could not
//! mark the redemption still enrolls — this is observability, and a Mongo
//! blip must not turn into a refused enrollment (`UsedTokenDao` is the
//! control; this is the count). Kept 90 days by the TTL on `issued_at`.

use bson::{DateTime, Document, doc, oid::ObjectId};
use mongodb::{Collection, Database};
use serde::{Deserialize, Serialize};

use super::base::DaoResult;

pub const ENROLL_TOKEN_ISSUED: &str = "enroll_token_issued";

/// How long a row is kept, from `issued_at` (spec §3p: 90 days).
pub const ENROLL_TOKEN_ISSUED_TTL_SECS: u64 = 90 * 24 * 60 * 60;

/// `kind` of a standard single-use token (the only kind recorded here).
pub const KIND_STANDARD: &str = "standard";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnrollTokenIssued {
    /// The token's `jti` — an id, never the token.
    #[serde(rename = "_id")]
    pub jti: String,
    pub tenant_id: ObjectId,
    /// The admin who minted it (an id, never an address).
    pub issued_by: ObjectId,
    pub issued_at: DateTime,
    pub kind: String,
    /// Set by the enroll that redeemed it; `null` until then.
    pub redeemed_at: Option<DateTime>,
    /// The device row the redemption created or revived; `null` until then.
    pub agent_id: Option<ObjectId>,
}

pub struct EnrollTokenIssuedDao {
    collection: Collection<EnrollTokenIssued>,
    raw: Collection<Document>,
}

impl EnrollTokenIssuedDao {
    pub fn new(db: &Database) -> Self {
        Self {
            collection: db.collection(ENROLL_TOKEN_ISSUED),
            raw: db.collection(ENROLL_TOKEN_ISSUED),
        }
    }

    /// Record a mint. A duplicate `jti` cannot happen (they are UUIDs); if
    /// one did, the first record wins and this is a no-op.
    pub async fn record_issued(
        &self,
        jti: &str,
        tenant_id: ObjectId,
        issued_by: ObjectId,
        issued_at: DateTime,
    ) -> DaoResult<()> {
        let row = EnrollTokenIssued {
            jti: jti.to_string(),
            tenant_id,
            issued_by,
            issued_at,
            kind: KIND_STANDARD.to_string(),
            redeemed_at: None,
            agent_id: None,
        };
        match self.collection.insert_one(&row).await {
            Ok(_) => Ok(()),
            Err(e) if is_duplicate_key(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Mark `jti` redeemed by `agent_id`. An UPSERT: a token minted before
    /// this collection existed has no row, and the claims it carries
    /// (`tenant_id`, `sub`, `iat`) are enough to write one at redemption, so
    /// "issued → redeemed" is complete across the deploy that introduced it.
    /// The first redemption wins; a replay never reaches here (the single-use
    /// ledger refuses it first).
    pub async fn mark_redeemed(
        &self,
        jti: &str,
        tenant_id: ObjectId,
        issued_by: ObjectId,
        issued_at: DateTime,
        agent_id: ObjectId,
        redeemed_at: DateTime,
    ) -> DaoResult<()> {
        self.raw
            .update_one(
                doc! { "_id": jti },
                doc! {
                    "$set": { "redeemed_at": redeemed_at, "agent_id": agent_id },
                    "$setOnInsert": {
                        "tenant_id": tenant_id,
                        "issued_by": issued_by,
                        "issued_at": issued_at,
                        "kind": KIND_STANDARD,
                    },
                },
            )
            .upsert(true)
            .await?;
        Ok(())
    }

    /// One row by its jti — for tests and the odd support question.
    pub async fn get(&self, jti: &str) -> DaoResult<Option<EnrollTokenIssued>> {
        Ok(self.collection.find_one(doc! { "_id": jti }).await?)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The row is a struct on the insert path, so a field added later
    /// travels; and `redeemed_at` / `agent_id` are present as `null` until a
    /// redemption, which is what a `{$ne: null}` read counts on.
    #[test]
    fn a_fresh_row_carries_explicit_nulls_and_round_trips() {
        let row = EnrollTokenIssued {
            jti: "a".repeat(32),
            tenant_id: ObjectId::new(),
            issued_by: ObjectId::new(),
            issued_at: DateTime::now(),
            kind: KIND_STANDARD.into(),
            redeemed_at: None,
            agent_id: None,
        };
        let doc = bson::to_document(&row).unwrap();
        assert_eq!(doc.get_str("_id").unwrap(), row.jti);
        assert_eq!(doc.get("redeemed_at"), Some(&bson::Bson::Null));
        assert_eq!(doc.get("agent_id"), Some(&bson::Bson::Null));
        assert!(doc.get("token").is_none() && doc.get("enrollment_token").is_none());
        let back: EnrollTokenIssued = bson::from_document(doc).unwrap();
        assert_eq!(back, row);
    }
}

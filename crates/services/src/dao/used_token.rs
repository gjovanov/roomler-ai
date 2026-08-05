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
    /// `Err(DaoError::Validation)` = it was already spent.
    ///
    /// A Mongo outage FAILS OPEN with a loud warning: enrollment is how a
    /// fleet recovers, and bricking every enroll because the ledger is
    /// unreachable trades a narrow replay window for a total outage. The
    /// token's short TTL remains the backstop.
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
                    tracing::warn!(
                        %purpose, %e,
                        "single-use token ledger unavailable; allowing the enrollment \
                         (short token TTL is the remaining bound)"
                    );
                    Ok(())
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

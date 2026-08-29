// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use bson::{DateTime, doc};
use mongodb::Database;
use rand::RngCore;
use roomler_ai_db::models::Subscriber;

use super::base::{BaseDao, DaoResult};

/// How long before a confirmation may be sent to the same address again.
///
/// ⚠️ This is the only thing standing between a public, unauthenticated form
/// and a mail bomb: without it, submitting a stranger's address in a loop sends
/// them one email per request, from our own domain — so the deliverability
/// damage lands on us, not on whoever ran the loop. The per-IP limiter does not
/// cover it, because the interesting attack is distributed and the victim is
/// the address, not the endpoint.
const RESEND_COOLDOWN_SECS: i64 = 15 * 60;

/// What actually happened, so the caller can decide whether to send mail.
///
/// ⚠️ This must never reach the HTTP response. `POST /api/subscribe` answers
/// 202 for every arm — a response that distinguishes `Created` from
/// `AlreadyConfirmed` is an oracle that reveals whether an address is on the
/// list, and the same address is very often a `users.email`, which is a unique
/// index AND the account-linking key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscribeOutcome {
    /// A new row. Send the confirmation.
    Created,
    /// Known but never confirmed — resend, so a lost first mail is recoverable.
    PendingResend,
    /// Already on the list and confirmed. Send nothing; re-mailing a confirmed
    /// subscriber every time a form is submitted is how a sender gets reported.
    AlreadyConfirmed,
    /// Previously unsubscribed, and has now asked again. Treated as fresh
    /// consent: the row is revived UNCONFIRMED with a new confirm token, so
    /// re-entry always costs a deliberate click.
    Resubscribed,
}

pub struct SubscriberDao {
    pub base: BaseDao<Subscriber>,
}

fn random_token() -> String {
    let mut b = [0u8; 24];
    rand::rng().fill_bytes(&mut b);
    // Hex, not base64: these travel in a URL path, and the standard base64
    // alphabet's `+` and `/` do not survive that without escaping.
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl SubscriberDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, Subscriber::COLLECTION),
        }
    }

    /// Idempotent by email. Returns what happened AND the confirm token to mail,
    /// if one should be mailed.
    pub async fn subscribe(
        &self,
        email: &str,
        source: &str,
    ) -> DaoResult<(SubscribeOutcome, Option<String>)> {
        let email = Subscriber::normalize_email(email);

        if let Some(existing) = self.base.find_one(doc! { "email": &email }).await? {
            let id = existing.id.expect("stored subscriber has an id");

            if existing.unsubscribed_at.is_some() {
                // Fresh consent after a withdrawal. Deliberately NOT restored to
                // `confirmed`: whatever the old state was, re-entry is a new
                // decision and has to be re-proved.
                let token = random_token();
                self.base
                    .update_by_id(
                        id,
                        doc! {
                            "$set": {
                                "confirmed": false,
                                "confirm_token": &token,
                                "confirm_sent_at": DateTime::now(),
                                "source": source,
                                "created_at": DateTime::now(),
                            },
                            "$unset": { "unsubscribed_at": "", "confirmed_at": "" },
                        },
                    )
                    .await?;
                return Ok((SubscribeOutcome::Resubscribed, Some(token)));
            }

            if existing.confirmed {
                return Ok((SubscribeOutcome::AlreadyConfirmed, None));
            }

            // Known, unconfirmed. Resending is right — the usual cause of a
            // stuck row is a first mail that never arrived — but only outside
            // the cooldown, or this branch IS the mail bomb.
            let cooled = existing
                .confirm_sent_at
                .map(|t| {
                    DateTime::now().timestamp_millis() - t.timestamp_millis()
                        >= RESEND_COOLDOWN_SECS * 1000
                })
                .unwrap_or(true);
            if !cooled {
                return Ok((SubscribeOutcome::PendingResend, None));
            }

            let token = match existing.confirm_token {
                Some(t) => t,
                None => random_token(),
            };
            self.base
                .update_by_id(
                    id,
                    doc! { "$set": { "confirm_token": &token, "confirm_sent_at": DateTime::now() } },
                )
                .await?;
            return Ok((SubscribeOutcome::PendingResend, Some(token)));
        }

        let confirm_token = random_token();
        let sub = Subscriber {
            id: None,
            email,
            source: source.to_string(),
            confirmed: false,
            confirm_token: Some(confirm_token.clone()),
            confirm_sent_at: Some(DateTime::now()),
            unsubscribe_token: random_token(),
            created_at: DateTime::now(),
            confirmed_at: None,
            unsubscribed_at: None,
        };
        self.base.insert_one(&sub).await?;
        Ok((SubscribeOutcome::Created, Some(confirm_token)))
    }

    /// Single-use: the token is cleared, so a link in a forwarded email cannot
    /// re-confirm an address its owner has since unsubscribed.
    pub async fn confirm(&self, token: &str) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! { "confirm_token": token },
                doc! {
                    "$set": { "confirmed": true, "confirmed_at": DateTime::now() },
                    "$unset": { "confirm_token": "" },
                },
            )
            .await
    }

    /// The row is kept and stamped, never deleted — see the model. Idempotent,
    /// because a mail client that prefetches links will call this more than once.
    pub async fn unsubscribe(&self, token: &str) -> DaoResult<bool> {
        self.base
            .update_one(
                doc! { "unsubscribe_token": token },
                doc! {
                    "$set": { "confirmed": false, "unsubscribed_at": DateTime::now() },
                    "$unset": { "confirm_token": "" },
                },
            )
            .await
    }

    /// Confirmed and not withdrawn — the only set that may be mailed.
    ///
    /// ⚠️ `"unsubscribed_at": null`, deliberately **not** `{"$type": "null"}`.
    /// The field is `skip_serializing_if = "Option::is_none"`, so it is ABSENT
    /// on a normal row rather than explicitly null; `$type: "null"` matches only
    /// an explicit null and would return an empty list forever — a mailing list
    /// that silently has no members. The `$type` form is right for the partial
    /// *index* filters in `indexes.rs`, and wrong here; the two are not
    /// interchangeable.
    pub async fn mailable(&self) -> DaoResult<Vec<Subscriber>> {
        self.base
            .find_many(
                doc! { "confirmed": true, "unsubscribed_at": null },
                Some(doc! { "created_at": -1 }),
            )
            .await
    }
}

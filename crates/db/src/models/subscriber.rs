// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use bson::{DateTime, oid::ObjectId};
use serde::{Deserialize, Serialize};

/// FR-39 — someone who asked to hear about the product without creating an
/// account.
///
/// Deliberately NOT a `User`. A subscriber has no password, no tenant, no
/// permissions and no session; folding the two would put an unauthenticated
/// public route on the write path of the collection whose `email` unique index
/// is the account-linking key (see the email-ownership invariant in
/// `CLAUDE.md`), which is exactly the surface that invariant exists to protect.
///
/// ⚠️ Both tokens are minted at **subscribe** time. Minting the unsubscribe
/// token when the first campaign is sent is how an address ends up on a list it
/// cannot leave — and a list nobody can leave is not a list that may lawfully
/// be sent to. Building the exit alongside the entrance is the only ordering
/// that cannot be forgotten later.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscriber {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,

    /// Lowercased and trimmed before storage, and uniquely indexed. The
    /// normalisation is load-bearing: without it `A@b.com` and `a@b.com` are two
    /// rows, and the second one cannot be unsubscribed by the first one's link.
    pub email: String,

    /// Where the address came from — `landing`, `docs`, a campaign tag. Clamped
    /// to a short allowlist-shaped string by the route, because it is
    /// caller-supplied and ends up in operator-facing exports.
    pub source: String,

    /// Double opt-in. `false` until the confirm link is followed; a row that
    /// never confirms is still a row, so a broken mail path does not lose the
    /// address.
    pub confirmed: bool,

    /// Capability for the confirm link. Unguessable; cleared on use so the link
    /// is single-use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm_token: Option<String>,

    /// Capability for the unsubscribe link. Never expires and is never cleared —
    /// a stale link in a two-year-old email must still work.
    pub unsubscribe_token: String,

    /// When a confirmation was last handed to the mailer. Load-bearing, not
    /// bookkeeping: without it, anyone can submit a stranger's address in a
    /// loop and this endpoint becomes a mail bomb aimed at that stranger, sent
    /// from our own domain. The resend cooldown reads this field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm_sent_at: Option<DateTime>,

    pub created_at: DateTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmed_at: Option<DateTime>,
    /// Set on unsubscribe. The row is KEPT rather than deleted: deleting it
    /// would let the same address be re-added silently by the next form
    /// submission, and there would be no record that consent was withdrawn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsubscribed_at: Option<DateTime>,
}

impl Subscriber {
    pub const COLLECTION: &'static str = "subscribers";

    /// The one place an address is normalised. Call it on every read path too —
    /// a lookup that skips it silently misses the row it was looking for.
    pub fn normalize_email(raw: &str) -> String {
        raw.trim().to_lowercase()
    }
}

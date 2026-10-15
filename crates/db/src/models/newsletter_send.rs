// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use bson::{DateTime, oid::ObjectId};
use serde::{Deserialize, Serialize};

/// FR-58 — one recipient's disposition for one issue: the delivery ledger.
///
/// 🔑 The unique index on `{issue_id, subscriber_id}` is the send program's
/// correctness invariant. Rows are inserted (`claimed`) BEFORE the send
/// attempt — email is the canonical at-most-once workload: a duplicate
/// newsletter is a visible spam-report event, a missed recipient is a
/// detectable stuck row. Even if two pods ever fanned out concurrently, the
/// per-recipient insert race resolves to one winner; the issue-level claim is
/// only an efficiency gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsletterSend {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,

    pub issue_id: ObjectId,
    /// The identity key — subscriber rows are never deleted, so it is stable.
    pub subscriber_id: ObjectId,
    /// Audit snapshot of what was actually mailed, so status can report
    /// addresses without a join and normalization drift can't blur history.
    pub email: String,

    pub status: SendStatus,
    /// Why a `failed` row failed — backend error text, redaction-free (it is
    /// our own mailer's error about our own request).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,

    pub claimed_at: DateTime,
    /// Refreshed by every mark; a row stuck `claimed` with an old
    /// `updated_at` is the STALE signature (crash between the backend's
    /// accept and our mark — genuinely ambiguous, reported, never silently
    /// retried).
    pub updated_at: DateTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<DateTime>,

    /// FR-102 P5 (D6) — what SendGrid's signed Event Webhook reported for
    /// this message after it was accepted. `None` until an event arrives.
    /// ⚠️ Separate from `status` on purpose: `status` is OUR disposition
    /// (claimed → sent/failed/suppressed) and the at-most-once ledger;
    /// `delivery` is the mailbox provider's answer, which arrives later,
    /// possibly out of order, possibly twice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<NewsletterDelivery>,
}

/// FR-102 P5 — the four SendGrid events the newsletter records, each as
/// the EARLIEST time it was reported (a replayed or duplicated event writes
/// nothing new). Opens and clicks are never recorded (D6): the footer's
/// promise is that nothing in the email is per-reader except the
/// unsubscribe link, and the server keeps nothing per reader either.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NewsletterDelivery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<DateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounced_at: Option<DateTime>,
    /// SendGrid's bounce `type`: `bounce` (hard — the address is suppressed)
    /// or `blocked` (the receiving server refused this message).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounce_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_at: Option<DateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spam_reported_at: Option<DateTime>,
    /// The bounce or drop reason as SendGrid gave it, cut to 200 characters
    /// (it is the receiving server's text about this one message).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// ⚠️ `Sent` means "accepted by the mail backend" (a SendGrid 202), not
/// delivered — the status surface phrases it that way on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendStatus {
    Claimed,
    Sent,
    Failed,
    /// Withdrawn (or no longer confirmed) between the snapshot and the send —
    /// the per-recipient re-check honored it.
    Suppressed,
}

impl NewsletterSend {
    pub const COLLECTION: &'static str = "newsletter_sends";
}

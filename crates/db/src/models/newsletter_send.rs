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

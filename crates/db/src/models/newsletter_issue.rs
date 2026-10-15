// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use bson::{DateTime, oid::ObjectId};
use serde::{Deserialize, Serialize};

/// FR-58 — one newsletter issue: the operator-authored markdown source plus
/// the metadata the branded email wrapper needs, and the send lifecycle.
///
/// The canonical source of an issue is a `.md` file (the same file is the
/// Medium post); this row is what the send pipeline works from. `body_md` is
/// rendered server-side with raw-HTML events dropped, so the only HTML in a
/// sent email is renderer-emitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsletterIssue {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,

    /// Stable handle, uniquely indexed (`[a-z0-9-]`, ≤64). Create is explicit
    /// and update is filtered to drafts — a typo'd slug must 404, never mint a
    /// second issue.
    pub slug: String,

    /// Email subject. Control characters are stripped at the route — a `\r\n`
    /// in a subject is SMTP header injection on the lettre backend.
    pub subject: String,
    /// Inbox preview line.
    pub preheader: String,
    /// The markdown body — the canonical content.
    pub body_md: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub hero_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hero_alt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cta_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cta_url: Option<String>,

    /// FR-102 P5 — a TEST issue's audience: the send goes only to the
    /// confirmed subscribers among these addresses (normalised, at most 20),
    /// never to the list. Fixed with the draft, so a resumed send can never
    /// widen it to everyone. `None` — every issue but a test — mails the
    /// list. Admin-only: the growth views see its size, never the addresses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<Vec<String>>,

    pub status: IssueStatus,

    /// Which pod's fan-out task holds the send claim (P3). The claim is an
    /// efficiency gate only — per-recipient correctness lives in the
    /// `newsletter_sends` unique index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<DateTime>,

    /// Stamped at completion. `None` while drafting/sending — live counts come
    /// from the ledger, stored counts only once the run terminated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counts: Option<IssueCounts>,

    pub created_at: DateTime,
    pub updated_at: DateTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<DateTime>,

    /// FR-102 P5 (D5) — the `utm_campaign` the issue's links to this site
    /// carried (`nl-<slug>`), stamped when the fan-out starts. `None` for an
    /// issue sent before the tags existed or with them switched off: its
    /// visits cannot be told apart, so they read "not measured".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub campaign: Option<String>,
    /// FR-102 P5 (D6) — the issue's delivery events are recorded: the
    /// fan-out tagged every message with the issue for SendGrid's Event
    /// Webhook, and this server verified the webhook when it sent. `false`
    /// for an issue sent before P5 or without the webhook key: its delivery
    /// is not measured, never 0.
    #[serde(default)]
    pub delivery_events: bool,
}

/// ⚠️ The terminal state is `Completed`, never "sent": an issue whose every
/// recipient failed still terminates, and calling that state "sent" would
/// lie. The counts carry the delivery truth — and ledger "sent" itself means
/// "accepted by the mail backend", which is not delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IssueStatus {
    #[default]
    Draft,
    Sending,
    Completed,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct IssueCounts {
    pub total: i64,
    pub sent: i64,
    pub failed: i64,
    pub suppressed: i64,
    /// Rows stuck `claimed` — a crash between the backend's accept and our
    /// mark. Genuinely ambiguous ("may or may not have received it"); reported,
    /// never silently retried.
    pub stale: i64,
}

impl NewsletterIssue {
    pub const COLLECTION: &'static str = "newsletter_issues";
}

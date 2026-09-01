// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-58 — the newsletter fan-out. One task per claimed send, spawned by the
//! send route on the pod that won the issue-level CAS.
//!
//! Deliberately NOT `TaskService` (tenant-scoped, no retry/recovery, and its
//! progress record would duplicate the ledger — which IS the progress record)
//! and NOT Redis `try_claim` (skip-a-cycle semantics; a send must not skip).
//! Coordination is Mongo CAS + the `newsletter_sends` unique index, so it
//! survives a Redis outage and a resume can land on either pod.
//!
//! ⚠️ A rolling deploy kills this task silently (bare `tokio::spawn`, no
//! drain). That is accepted and documented: status then shows `sending` with
//! a stale heartbeat, and recovery is the operator re-POSTing send (the
//! stale-claim arm of the CAS). Don't deploy mid-send.

use std::sync::Arc;
use std::time::Duration;

use bson::DateTime;
use tokio::sync::Semaphore;
use tracing::{info, warn};

use roomler_ai_db::models::{NewsletterIssue, Subscriber};
use roomler_ai_services::email::SendOptions;
use roomler_ai_services::newsletter::{render_issue_html, substitute_recipient};

use crate::routes::newsletter::{newsletter_from, unsubscribe_headers, unsubscribe_url};
use crate::state::AppState;

/// A ledger row still `claimed` after this long is STALE — the crash-window
/// residue the status surface reports and `retry_stale` may re-attempt.
pub const STALE_ROW_SECS: i64 = 15 * 60;
/// An issue claim older than this may be re-claimed by a resume POST. Kept
/// well above the heartbeat interval so a live fan-out is never usurped.
pub const ISSUE_CLAIM_STALE_SECS: i64 = 10 * 60;

const SEND_CONCURRENCY: usize = 4;
/// ⚠️ Load-bearing, not polish: the SendGrid `reqwest::Client::new()` has NO
/// default timeout, so one hung connection would wedge a semaphore slot — and
/// eventually the whole fan-out — behind a live heartbeat masking it.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);

pub fn stale_cutoff() -> DateTime {
    DateTime::from_millis(DateTime::now().timestamp_millis() - STALE_ROW_SECS * 1000)
}

/// The fan-out. Pre-renders once, then per recipient: claim (unique index
/// arbitrates) → re-check consent → substitute → send with a hard timeout →
/// mark. Finishes by stamping `completed` + the counts snapshot.
pub async fn run_send(state: AppState, issue: NewsletterIssue, retry_stale: bool) {
    let slug = issue.slug.clone();
    let issue_id = issue.id.expect("a stored issue has an id");
    let Some(mailer) = state.email.clone() else {
        // The route refuses before claiming; this is the defensive twin.
        warn!(%slug, "run_send started with no mailer — leaving the claim to go stale");
        return;
    };

    let subscribers = match state.subscribers.mailable().await {
        Ok(v) => v,
        Err(e) => {
            // Leave the issue `sending`; the heartbeat stops with us and the
            // stale claim makes a re-POST the recovery path.
            warn!(%slug, error = ?e, "run_send could not load the recipient snapshot");
            return;
        }
    };

    let rendered = render_issue_html(&issue);
    let from = newsletter_from(&state);
    let subject = issue.subject.clone();

    // The claim stays visibly alive while we work; scoped to our pod id.
    let hb = {
        let issues = state.newsletter_issues.clone();
        let pod = state.pod.pod_id.clone();
        let slug = slug.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(HEARTBEAT_EVERY);
            tick.tick().await; // the immediate first tick — the claim is fresh
            loop {
                tick.tick().await;
                if let Err(e) = issues.heartbeat(&slug, &pod).await {
                    warn!(%slug, error = ?e, "send heartbeat failed");
                }
            }
        })
    };

    // Phase A — explicitly requested retries of stale rows. Per-row CAS, so a
    // second resuming pod can't double-attempt one recipient; the re-fetch
    // honors a withdrawal that happened while the row sat stuck.
    let mut work: Vec<(bson::oid::ObjectId, Subscriber)> = Vec::new();
    if retry_stale {
        match state
            .newsletter_sends
            .stale_rows(issue_id, stale_cutoff())
            .await
        {
            Ok(rows) => {
                for row in rows {
                    let row_id = row.id.expect("a stored ledger row has an id");
                    match state.newsletter_sends.reclaim(row_id, stale_cutoff()).await {
                        Ok(true) => {
                            match state.subscribers.base.find_by_id(row.subscriber_id).await {
                                Ok(sub) => work.push((row_id, sub)),
                                Err(_) => {
                                    // The subscriber row is unreadable — treat as
                                    // withdrawn rather than guessing.
                                    let _ = state.newsletter_sends.mark_suppressed(row_id).await;
                                }
                            }
                        }
                        Ok(false) => {} // someone else owns the retry
                        Err(e) => warn!(%slug, error = ?e, "stale-row reclaim failed"),
                    }
                }
            }
            Err(e) => warn!(%slug, error = ?e, "could not list stale rows"),
        }
    }

    // Phase B — the snapshot. Claim-first; a duplicate claim means an earlier
    // pass already dispositioned the recipient (that is the resume working).
    for sub in subscribers {
        let sub_id = sub.id.expect("a stored subscriber has an id");
        match state
            .newsletter_sends
            .try_claim(issue_id, sub_id, &sub.email)
            .await
        {
            Ok(Some(row_id)) => work.push((row_id, sub)),
            Ok(None) => {}
            Err(e) => warn!(%slug, error = ?e, "ledger claim failed; recipient skipped this pass"),
        }
    }

    let sem = Arc::new(Semaphore::new(SEND_CONCURRENCY));
    let mut handles = Vec::with_capacity(work.len());
    for (row_id, sub) in work {
        let permit = sem.clone().acquire_owned().await.expect("semaphore open");
        let state = state.clone();
        let mailer = mailer.clone();
        let rendered = rendered.clone();
        let subject = subject.clone();
        let from = from.clone();
        handles.push(tokio::spawn(async move {
            let _permit = permit;

            // The consent re-check: honored between snapshot and send.
            let fresh = state.subscribers.base.find_by_id(sub.id.unwrap()).await;
            let withdrawn = match &fresh {
                Ok(f) => f.unsubscribed_at.is_some() || !f.confirmed,
                Err(_) => true, // unreadable ⇒ do not mail
            };
            if withdrawn {
                let _ = state.newsletter_sends.mark_suppressed(row_id).await;
                return;
            }

            let unsub = unsubscribe_url(&state, &sub.unsubscribe_token);
            let html = substitute_recipient(&rendered, &unsub);
            let opts = SendOptions {
                headers: unsubscribe_headers(&unsub),
                from,
            };
            let outcome = tokio::time::timeout(
                SEND_TIMEOUT,
                mailer.send_ext(&sub.email, &subject, &html, &opts),
            )
            .await;
            let mark = match outcome {
                Ok(Ok(())) => state.newsletter_sends.mark_sent(row_id).await,
                Ok(Err(e)) => {
                    state
                        .newsletter_sends
                        .mark_failed(row_id, &format!("{e:#}"))
                        .await
                }
                Err(_) => {
                    state
                        .newsletter_sends
                        .mark_failed(row_id, "send timed out after 30s")
                        .await
                }
            };
            if let Err(e) = mark {
                warn!(error = ?e, "ledger mark failed — the row will read as stale");
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    hb.abort();

    match state
        .newsletter_sends
        .counts(issue_id, stale_cutoff())
        .await
    {
        Ok(counts) => {
            if let Err(e) = state.newsletter_issues.complete(&slug, counts).await {
                warn!(%slug, error = ?e, "could not stamp completion");
            } else {
                info!(
                    %slug,
                    total = counts.total,
                    sent = counts.sent,
                    failed = counts.failed,
                    suppressed = counts.suppressed,
                    stale = counts.stale,
                    "newsletter fan-out completed"
                );
            }
        }
        Err(e) => warn!(%slug, error = ?e, "could not compute completion counts"),
    }
}

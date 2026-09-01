// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-58 — the newsletter sending program's admin surface (issues CRUD,
//! preview, test-send; the fan-out itself is P3).
//!
//! Every handler's first line is `require_platform_admin` — the existing
//! ObjectId allowlist, answering **404** on missing authority (never 403: the
//! web client force-logs-out on 403, and a hidden surface beats an
//! acknowledged one). `platform_admins` unset ⇒ this entire surface 404s;
//! that inherent gate is the kill switch, and a second config flag guarding
//! the same door would just be a second switch to forget.
//!
//! The preview IS the sent artifact: it serves the exact bytes
//! `render_issue_html` produces (with a sample unsubscribe URL substituted),
//! because the fan-out pre-renders once and substitutes per recipient.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use bson::DateTime;
use serde::{Deserialize, Serialize};

use roomler_ai_db::models::{IssueCounts, IssueStatus, NewsletterIssue};
use roomler_ai_services::email::SendOptions;
use roomler_ai_services::newsletter::{clean_header_text, render_issue_html, substitute_recipient};

use super::stats::require_platform_admin;
use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};

/// Storage guard for the markdown body. An issue is an email, not a book.
const MAX_BODY_MD_BYTES: usize = 256 * 1024;

// ── Wire shapes ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct IssueBody {
    pub subject: String,
    pub preheader: String,
    pub body_md: String,
    #[serde(default)]
    pub hero_url: Option<String>,
    #[serde(default)]
    pub hero_alt: Option<String>,
    #[serde(default)]
    pub cta_text: Option<String>,
    #[serde(default)]
    pub cta_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateIssue {
    pub slug: String,
    #[serde(flatten)]
    pub body: IssueBody,
}

/// Read model. Plain strings and numbers only — a raw `bson::DateTime`
/// serialises as `{"$date":…}`, a truthy object that renders as
/// `[object Object]` (the FR-12 wire-leak lesson).
#[derive(Debug, Serialize)]
pub struct IssueView {
    pub slug: String,
    pub subject: String,
    pub preheader: String,
    pub body_md: String,
    pub hero_url: Option<String>,
    pub hero_alt: Option<String>,
    pub cta_text: Option<String>,
    pub cta_url: Option<String>,
    pub status: IssueStatus,
    pub created_at: String,
    pub updated_at: String,
    pub sent_at: Option<String>,
    pub counts: Option<IssueCounts>,
}

#[derive(Debug, Serialize)]
pub struct IssueListItem {
    pub slug: String,
    pub subject: String,
    pub status: IssueStatus,
    pub created_at: String,
    pub sent_at: Option<String>,
    pub counts: Option<IssueCounts>,
}

fn rfc3339(t: DateTime) -> String {
    t.try_to_rfc3339_string().unwrap_or_default()
}

fn view(i: NewsletterIssue) -> IssueView {
    IssueView {
        slug: i.slug,
        subject: i.subject,
        preheader: i.preheader,
        body_md: i.body_md,
        hero_url: i.hero_url,
        hero_alt: i.hero_alt,
        cta_text: i.cta_text,
        cta_url: i.cta_url,
        status: i.status,
        created_at: rfc3339(i.created_at),
        updated_at: rfc3339(i.updated_at),
        sent_at: i.sent_at.map(rfc3339),
        counts: i.counts,
    }
}

// ── Validation ──────────────────────────────────────────────────────────

fn valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Absolute http(s), no quote/angle/space/control — these land in `src`/`href`
/// attributes, and the escaping downstream is belt, not license.
fn valid_http_url(s: &str) -> bool {
    (s.starts_with("https://") || s.starts_with("http://"))
        && s.len() <= 512
        && !s
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '"' || c == '<' || c == '>')
}

struct CleanIssueBody {
    subject: String,
    preheader: String,
    body_md: String,
    hero_url: Option<String>,
    hero_alt: Option<String>,
    cta_text: Option<String>,
    cta_url: Option<String>,
}

fn clean_issue_body(b: IssueBody) -> Result<CleanIssueBody, ApiError> {
    // ⚠️ Control chars stripped here because a `\r\n` in a subject is SMTP
    // header injection on the lettre backend — this is a security bound, not
    // tidiness.
    let subject = clean_header_text(&b.subject, 200);
    let preheader = clean_header_text(&b.preheader, 300);
    if subject.is_empty() {
        return Err(ApiError::Validation("subject must not be empty".into()));
    }
    if b.body_md.len() > MAX_BODY_MD_BYTES {
        return Err(ApiError::Validation(format!(
            "body_md exceeds {MAX_BODY_MD_BYTES} bytes"
        )));
    }
    for url in [&b.hero_url, &b.cta_url].into_iter().flatten() {
        if !valid_http_url(url) {
            return Err(ApiError::Validation(format!(
                "not an absolute http(s) URL: {url:?}"
            )));
        }
    }
    if b.cta_text.is_some() != b.cta_url.is_some() {
        return Err(ApiError::Validation(
            "cta_text and cta_url come together or not at all".into(),
        ));
    }
    Ok(CleanIssueBody {
        subject,
        preheader,
        body_md: b.body_md,
        hero_url: b.hero_url,
        hero_alt: b.hero_alt.map(|s| clean_header_text(&s, 200)),
        cta_text: b.cta_text.map(|s| clean_header_text(&s, 64)),
        cta_url: b.cta_url,
    })
}

// ── The newsletter From + one-click headers (shared with the P3 fan-out) ──

/// The newsletter From mailbox: `newsletter.from_*` with per-field fallback to
/// the transactional `email.from_*`. `None` = no override at all (both unset).
pub(crate) fn newsletter_from(state: &AppState) -> Option<(String, String)> {
    let nl = &state.settings.newsletter;
    let em = &state.settings.email;
    match (nl.from_email.clone(), nl.from_name.clone()) {
        (None, None) => None,
        (e, n) => Some((
            e.unwrap_or_else(|| em.from_email.clone()),
            n.unwrap_or_else(|| em.from_name.clone()),
        )),
    }
}

/// RFC 8058: both headers, or providers show no one-click button.
pub(crate) fn unsubscribe_headers(unsubscribe_url: &str) -> Vec<(String, String)> {
    vec![
        ("List-Unsubscribe".into(), format!("<{unsubscribe_url}>")),
        (
            "List-Unsubscribe-Post".into(),
            "List-Unsubscribe=One-Click".into(),
        ),
    ]
}

pub(crate) fn unsubscribe_url(state: &AppState, token: &str) -> String {
    let base = state.settings.app.frontend_url.trim_end_matches('/');
    format!("{base}/api/subscribe/unsubscribe/{token}")
}

// ── Handlers ────────────────────────────────────────────────────────────

/// `POST /api/admin/newsletter/issues`
pub async fn create(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<CreateIssue>,
) -> Result<impl IntoResponse, ApiError> {
    require_platform_admin(&state, &auth)?;
    if !valid_slug(&body.slug) {
        return Err(ApiError::Validation(
            "slug must be [a-z0-9-], at most 64 chars".into(),
        ));
    }
    let clean = clean_issue_body(body.body)?;
    let issue = NewsletterIssue {
        id: None,
        slug: body.slug,
        subject: clean.subject,
        preheader: clean.preheader,
        body_md: clean.body_md,
        hero_url: clean.hero_url,
        hero_alt: clean.hero_alt,
        cta_text: clean.cta_text,
        cta_url: clean.cta_url,
        status: IssueStatus::Draft,
        claimed_by: None,
        claimed_at: None,
        counts: None,
        created_at: DateTime::now(),
        updated_at: DateTime::now(),
        sent_at: None,
    };
    // Two concurrent creates on one slug: the unique index arbitrates and the
    // loser's DuplicateKey maps to 409.
    state.newsletter_issues.create(&issue).await?;
    Ok((StatusCode::CREATED, Json(view(issue))))
}

/// `PUT /api/admin/newsletter/issues/{slug}` — drafts only.
pub async fn update(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(slug): Path<String>,
    Json(body): Json<IssueBody>,
) -> Result<Json<IssueView>, ApiError> {
    require_platform_admin(&state, &auth)?;
    let clean = clean_issue_body(body)?;
    let mut set = bson::doc! {
        "subject": &clean.subject,
        "preheader": &clean.preheader,
        "body_md": &clean.body_md,
    };
    // Optional fields are SET or UNSET explicitly — a hand-built doc that
    // skips the None arm would silently keep a hero the operator removed.
    let mut unset = bson::Document::new();
    for (key, value) in [
        ("hero_url", &clean.hero_url),
        ("hero_alt", &clean.hero_alt),
        ("cta_text", &clean.cta_text),
        ("cta_url", &clean.cta_url),
    ] {
        match value {
            Some(v) => {
                set.insert(key, v);
            }
            None => {
                unset.insert(key, "");
            }
        }
    }
    let matched = if unset.is_empty() {
        state.newsletter_issues.update_draft(&slug, set).await?
    } else {
        // update_draft only $sets; do the combined form here.
        set.insert("updated_at", DateTime::now());
        state
            .newsletter_issues
            .base
            .update_one(
                bson::doc! { "slug": &slug, "status": "draft" },
                bson::doc! { "$set": set, "$unset": unset },
            )
            .await?
    };
    if !matched {
        // Absent vs no-longer-a-draft have different fixes; say which.
        return match state.newsletter_issues.get_by_slug(&slug).await? {
            None => Err(ApiError::NotFound("no such issue".into())),
            Some(_) => Err(ApiError::Conflict(
                "issue is no longer a draft and cannot be edited".into(),
            )),
        };
    }
    let issue = state
        .newsletter_issues
        .get_by_slug(&slug)
        .await?
        .ok_or_else(|| ApiError::NotFound("no such issue".into()))?;
    Ok(Json(view(issue)))
}

/// `GET /api/admin/newsletter/issues`
pub async fn list(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Vec<IssueListItem>>, ApiError> {
    require_platform_admin(&state, &auth)?;
    let items = state
        .newsletter_issues
        .list()
        .await?
        .into_iter()
        .map(|i| IssueListItem {
            slug: i.slug,
            subject: i.subject,
            status: i.status,
            created_at: rfc3339(i.created_at),
            sent_at: i.sent_at.map(rfc3339),
            counts: i.counts,
        })
        .collect();
    Ok(Json(items))
}

/// `GET /api/admin/newsletter/issues/{slug}`
pub async fn get_one(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(slug): Path<String>,
) -> Result<Json<IssueView>, ApiError> {
    require_platform_admin(&state, &auth)?;
    let issue = state
        .newsletter_issues
        .get_by_slug(&slug)
        .await?
        .ok_or_else(|| ApiError::NotFound("no such issue".into()))?;
    Ok(Json(view(issue)))
}

/// `GET /api/admin/newsletter/issues/{slug}/preview` — the exact send-path
/// bytes, with a sample unsubscribe URL substituted.
pub async fn preview(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(slug): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    require_platform_admin(&state, &auth)?;
    let issue = state
        .newsletter_issues
        .get_by_slug(&slug)
        .await?
        .ok_or_else(|| ApiError::NotFound("no such issue".into()))?;
    let html = substitute_recipient(
        &render_issue_html(&issue),
        &unsubscribe_url(&state, "preview-sample-token"),
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "text/html; charset=utf-8".parse().expect("static header"),
    );
    // Belt: the body is renderer-emitted only, but this response renders in
    // the admin's authenticated browser origin — sandbox it anyway.
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        "sandbox".parse().expect("static header"),
    );
    Ok((headers, html))
}

#[derive(Debug, Deserialize)]
pub struct TestSend {
    pub email: String,
}

#[derive(Debug, Serialize)]
pub struct TestSendResult {
    pub sent: bool,
    pub to: String,
}

/// `POST /api/admin/newsletter/issues/{slug}/test-send` — render + send to ONE
/// address with the real headers and a sample token. No ledger, no status
/// change; failures are propagated honestly (this caller is an operator, not
/// an oracle-sensitive public form).
pub async fn test_send(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(slug): Path<String>,
    Json(body): Json<TestSend>,
) -> Result<Json<TestSendResult>, ApiError> {
    require_platform_admin(&state, &auth)?;
    let issue = state
        .newsletter_issues
        .get_by_slug(&slug)
        .await?
        .ok_or_else(|| ApiError::NotFound("no such issue".into()))?;

    let Some(mailer) = state.email.clone() else {
        return Err(ApiError::BadRequest(
            "no mailer configured — set ROOMLER__EMAIL__API_KEY or SMTP host/port".into(),
        ));
    };
    let to = body.email.trim().to_string();
    if !to.contains('@') || to.len() > 254 {
        return Err(ApiError::Validation("not a plausible address".into()));
    }

    let unsub = unsubscribe_url(&state, "test-send-sample-token");
    let html = substitute_recipient(&render_issue_html(&issue), &unsub);
    let opts = SendOptions {
        headers: unsubscribe_headers(&unsub),
        from: newsletter_from(&state),
    };
    mailer
        .send_ext(&to, &issue.subject, &html, &opts)
        .await
        .map_err(|e| ApiError::Internal(format!("test-send failed: {e:#}")))?;
    Ok(Json(TestSendResult { sent: true, to }))
}

// ── P3: the fan-out ─────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub struct SendRequest {
    /// Re-attempt ledger rows stuck `claimed` past the staleness cutoff — an
    /// explicit operator decision, never automatic: a stuck row is "maybe
    /// delivered", and auto-retry converts that into "maybe delivered twice".
    #[serde(default)]
    pub retry_stale: bool,
}

#[derive(Debug, Serialize)]
pub struct SendAccepted {
    pub slug: String,
    pub status: IssueStatus,
    pub claimed_by: String,
}

/// `POST /api/admin/newsletter/issues/{slug}/send` — claim and fan out.
/// Re-POSTing is the RESUME path: the CAS re-claims a `sending` issue whose
/// claim went stale (dead pod), and the ledger makes the re-run idempotent.
pub async fn send(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(slug): Path<String>,
    body: Option<Json<SendRequest>>,
) -> Result<impl IntoResponse, ApiError> {
    require_platform_admin(&state, &auth)?;
    let req = body.map(|Json(b)| b).unwrap_or_default();

    // Refuse BEFORE claiming: ledgering an entire list into `failed` on a
    // misconfigured pod would be strictly worse than refusing loudly.
    if state.email.is_none() {
        return Err(ApiError::BadRequest(
            "no mailer configured — set ROOMLER__EMAIL__API_KEY or SMTP host/port".into(),
        ));
    }

    let claimed = state
        .newsletter_issues
        .claim_for_send(
            &slug,
            &state.pod.pod_id,
            crate::newsletter_send::ISSUE_CLAIM_STALE_SECS,
        )
        .await?;

    match claimed {
        Some(issue) => {
            let accepted = SendAccepted {
                slug: issue.slug.clone(),
                status: issue.status,
                claimed_by: state.pod.pod_id.clone(),
            };
            tokio::spawn(crate::newsletter_send::run_send(
                state.clone(),
                issue,
                req.retry_stale,
            ));
            Ok((StatusCode::ACCEPTED, Json(accepted)))
        }
        None => match state.newsletter_issues.get_by_slug(&slug).await? {
            None => Err(ApiError::NotFound("no such issue".into())),
            Some(i) => match i.status {
                IssueStatus::Completed => Err(ApiError::Conflict(
                    "issue already completed — one issue, one send; counts carry the truth".into(),
                )),
                IssueStatus::Sending => Err(ApiError::Conflict(format!(
                    "already sending on pod {} since {} — a live claim is never usurped; \
                     re-POST after it goes stale to resume",
                    i.claimed_by.as_deref().unwrap_or("?"),
                    i.claimed_at.map(rfc3339).unwrap_or_default(),
                ))),
                IssueStatus::Draft => Err(ApiError::Conflict(
                    "claim raced another caller — try again".into(),
                )),
            },
        },
    }
}

#[derive(Debug, Serialize)]
pub struct FailedRow {
    pub email: String,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct IssueStatusView {
    pub slug: String,
    pub status: IssueStatus,
    /// Live from the ledger while sending; the stored snapshot once
    /// completed. `sent` means "accepted by the mail backend", not delivered.
    pub counts: IssueCounts,
    /// Rows stuck `claimed` — these recipients MAY or MAY NOT have received
    /// the issue (crash between backend accept and our mark). Capped sample.
    pub stale_addresses: Vec<String>,
    /// Capped sample of failures with their backend errors.
    pub failed_sample: Vec<FailedRow>,
    pub claimed_by: Option<String>,
    pub claimed_at: Option<String>,
    pub sent_at: Option<String>,
}

/// `GET /api/admin/newsletter/issues/{slug}/status`
pub async fn status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(slug): Path<String>,
) -> Result<Json<IssueStatusView>, ApiError> {
    require_platform_admin(&state, &auth)?;
    let issue = state
        .newsletter_issues
        .get_by_slug(&slug)
        .await?
        .ok_or_else(|| ApiError::NotFound("no such issue".into()))?;
    let issue_id = issue.id.expect("a stored issue has an id");
    let cutoff = crate::newsletter_send::stale_cutoff();

    let counts = match (issue.status, issue.counts) {
        (IssueStatus::Completed, Some(stored)) => stored,
        _ => state.newsletter_sends.counts(issue_id, cutoff).await?,
    };
    let stale_addresses = state
        .newsletter_sends
        .stale_rows(issue_id, cutoff)
        .await?
        .into_iter()
        .take(20)
        .map(|r| r.email)
        .collect();
    let failed_sample = state
        .newsletter_sends
        .failed_sample(issue_id, 20)
        .await?
        .into_iter()
        .map(|r| FailedRow {
            email: r.email,
            error: r.error,
        })
        .collect();

    Ok(Json(IssueStatusView {
        slug: issue.slug,
        status: issue.status,
        counts,
        stale_addresses,
        failed_sample,
        claimed_by: issue.claimed_by,
        claimed_at: issue.claimed_at.map(rfc3339),
        sent_at: issue.sent_at.map(rfc3339),
    }))
}

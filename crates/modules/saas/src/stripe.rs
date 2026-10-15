// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use bson::oid::ObjectId;
use serde::Deserialize;

use roomler_ai_db::models::role::permissions;
use roomler_ai_services::stripe::{StripeEvent, StripeService};
use roomler_core::{ApiError, Core, extractors::auth::AuthUser};

// ---- Request types -------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CheckoutRequest {
    pub tenant_id: String,
    pub plan: String,
    pub success_url: String,
    pub cancel_url: String,
}

#[derive(Debug, Deserialize)]
pub struct PortalRequest {
    pub tenant_id: String,
    pub return_url: String,
}

// ---- GET /api/stripe/plans (public) --------------------------------------

pub async fn get_plans() -> Json<Vec<roomler_ai_services::stripe::PlanInfo>> {
    Json(StripeService::get_plans())
}

// ---- POST /api/stripe/checkout (authenticated, MANAGE_TENANT) ------------

pub async fn create_checkout(
    State(state): State<Core>,
    auth: AuthUser,
    Json(body): Json<CheckoutRequest>,
) -> Result<Json<roomler_ai_services::stripe::CheckoutResponse>, ApiError> {
    let tenant_id = parse_oid(&body.tenant_id)?;
    require_manage_tenant(&state, tenant_id, auth.user_id).await?;

    let stripe = StripeService::new(&state.settings.stripe);
    let result = stripe
        .create_checkout_session(
            &state.db,
            &tenant_id,
            &body.plan,
            &auth.email,
            &body.success_url,
            &body.cancel_url,
        )
        .await
        .map_err(stripe_err)?;

    Ok(Json(result))
}

// ---- POST /api/stripe/portal (authenticated, MANAGE_TENANT) --------------

pub async fn create_portal(
    State(state): State<Core>,
    auth: AuthUser,
    Json(body): Json<PortalRequest>,
) -> Result<Json<roomler_ai_services::stripe::PortalResponse>, ApiError> {
    let tenant_id = parse_oid(&body.tenant_id)?;
    require_manage_tenant(&state, tenant_id, auth.user_id).await?;

    let stripe = StripeService::new(&state.settings.stripe);
    let result = stripe
        .create_portal_session(&state.db, &tenant_id, &body.return_url)
        .await
        .map_err(stripe_err)?;

    Ok(Json(result))
}

// ---- POST /api/stripe/webhook (no auth, raw body) ------------------------

pub async fn webhook(
    State(state): State<Core>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let sig_header = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::BadRequest("Missing Stripe-Signature header".to_string()))?;

    // Verify signature
    StripeService::verify_signature(&state.settings.stripe.webhook_secret, &body, sig_header)
        .map_err(|_| ApiError::Unauthorized("Invalid webhook signature".to_string()))?;

    // Parse event
    let event: StripeEvent = serde_json::from_slice(&body)
        .map_err(|e| ApiError::BadRequest(format!("Invalid event payload: {e}")))?;

    // Process event
    let stripe = StripeService::new(&state.settings.stripe);
    stripe
        .handle_webhook_event(&state.db, &event)
        .await
        .map_err(stripe_err)?;

    Ok(StatusCode::OK)
}

// ---- FR-102 P4: the operator's one-off backfill ------------------------------

#[derive(Debug, Deserialize)]
pub struct BackfillQuery {
    /// `YYYY-MM-DD` (UTC): read invoices created since this day. Absent =
    /// all of them (bounded by `BACKFILL_MAX_PAGES` pages of 100).
    #[serde(default)]
    pub since: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct BackfillAnswer {
    #[serde(flatten)]
    pub report: roomler_ai_services::stripe::BackfillReport,
    /// The closed months refrozen from the rows now on record (`YYYY-MM`).
    pub refrozen: Vec<String>,
}

/// `POST /api/admin/growth/billing/backfill?since=YYYY-MM-DD` — FR-102 P4:
/// read Stripe's paid invoices into `billing_events` (idempotent: an
/// invoice already on record is left as it was), then refreeze the closed
/// months those invoices touch. Platform admins only — 404 otherwise and
/// when `growth.enabled = false`, like every growth route. One-off and
/// operator-run: it reads Stripe and never writes to it.
pub async fn backfill(
    State(state): State<Core>,
    auth: AuthUser,
    axum::extract::Query(q): axum::extract::Query<BackfillQuery>,
) -> Result<Json<BackfillAnswer>, ApiError> {
    roomler_core::guards::require_platform_admin(&state, &auth)?;
    if !state.settings.growth.enabled {
        return Err(ApiError::NotFound("Not found".to_string()));
    }
    let since_unix = match q.since.as_deref() {
        None => None,
        Some(raw) => {
            let day = chrono::NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d")
                .map_err(|_| ApiError::BadRequest("since must be YYYY-MM-DD".into()))?;
            let floor = chrono::NaiveDate::from_ymd_opt(2020, 1, 1).expect("a real day");
            if day < floor {
                return Err(ApiError::BadRequest(
                    "since must be 2020-01-01 or later".into(),
                ));
            }
            Some(
                day.and_hms_opt(0, 0, 0)
                    .expect("midnight exists")
                    .and_utc()
                    .timestamp(),
            )
        }
    };
    let now = bson::DateTime::now();
    let stripe = StripeService::new(&state.settings.stripe);
    let report = stripe
        .backfill_paid_invoices(
            &state.db,
            roomler_ai_services::stripe::STRIPE_API_BASE,
            since_unix,
            now,
        )
        .await
        .map_err(|e| match e {
            roomler_ai_services::stripe::StripeError::ApiError(m) => ApiError::BadRequest(m),
            other => stripe_err(other),
        })?;
    let refrozen = refreeze(&state, &report.months, now).await?;
    tracing::info!(
        admin = %auth.user_id, recorded = report.recorded, refrozen = refrozen.len(),
        "growth billing backfill"
    );
    Ok(Json(BackfillAnswer { report, refrozen }))
}

/// Rewrite the frozen totals of the CLOSED months a backfill touched (past
/// the freeze grace) — the one deliberate correction a frozen month takes.
/// The current month is never frozen; it is computed live.
pub async fn refreeze(
    state: &Core,
    months: &[String],
    now: bson::DateTime,
) -> Result<Vec<String>, ApiError> {
    use roomler_ai_services::billing;
    let excluded: Vec<ObjectId> = roomler_core::growth::ExclusionSet::load(state)
        .await?
        .tenants
        .into_iter()
        .collect();
    let now_ms = now.timestamp_millis();
    let mut done = Vec::new();
    for label in months {
        let Some((_, end)) = billing::month_bounds(label) else {
            continue;
        };
        if end + billing::FREEZE_GRACE_DAYS * 86_400_000 > now_ms {
            continue;
        }
        if billing::freeze_month(&state.db, label, &excluded, true, now).await? {
            done.push(label.clone());
        }
    }
    Ok(done)
}

// ---- Helpers -------------------------------------------------------------

fn parse_oid(s: &str) -> Result<ObjectId, ApiError> {
    ObjectId::parse_str(s).map_err(|_| ApiError::BadRequest(format!("Invalid ObjectId: {s}")))
}

async fn require_manage_tenant(
    state: &Core,
    tenant_id: ObjectId,
    user_id: ObjectId,
) -> Result<(), ApiError> {
    // Tenant owner always has access
    let tenant = state.tenants.base.find_by_id(tenant_id).await?;
    if tenant.owner_id == user_id {
        return Ok(());
    }

    // Otherwise check MANAGE_TENANT permission
    let perms = state
        .tenants
        .get_member_permissions(tenant_id, user_id)
        .await?;
    if !permissions::has(perms, permissions::MANAGE_TENANT) {
        return Err(ApiError::Forbidden(
            "Missing MANAGE_TENANT permission".to_string(),
        ));
    }
    Ok(())
}

fn stripe_err(e: roomler_ai_services::stripe::StripeError) -> ApiError {
    use roomler_ai_services::stripe::StripeError;
    match e {
        StripeError::TenantNotFound => ApiError::NotFound("Tenant not found".to_string()),
        StripeError::NoBillingAccount => {
            ApiError::BadRequest("No billing account for this tenant".to_string())
        }
        StripeError::InvalidPlan(p) => ApiError::BadRequest(format!("Invalid plan: {p}")),
        StripeError::PriceNotConfigured(p) => ApiError::ServiceUnavailable(format!(
            "Payments are not configured on this server (missing Stripe price id for plan '{p}')"
        )),
        StripeError::InvalidSignature => {
            ApiError::Unauthorized("Invalid webhook signature".to_string())
        }
        StripeError::ApiError(msg) => ApiError::Internal(format!("Stripe API error: {msg}")),
        StripeError::Mongo(e) => ApiError::Internal(e.to_string()),
        StripeError::Storage(msg) => ApiError::Internal(msg),
    }
}

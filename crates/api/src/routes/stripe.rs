use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use bson::oid::ObjectId;
use serde::Deserialize;

use crate::{
    error::ApiError,
    extractors::auth::AuthUser,
    state::AppState,
};
use roomler_ai_db::models::role::permissions;
use roomler_ai_services::stripe::{StripeEvent, StripeService};

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
    State(state): State<AppState>,
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
    State(state): State<AppState>,
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
    State(state): State<AppState>,
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

// ---- Helpers -------------------------------------------------------------

fn parse_oid(s: &str) -> Result<ObjectId, ApiError> {
    ObjectId::parse_str(s).map_err(|_| ApiError::BadRequest(format!("Invalid ObjectId: {s}")))
}

async fn require_manage_tenant(
    state: &AppState,
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
        StripeError::InvalidSignature => {
            ApiError::Unauthorized("Invalid webhook signature".to_string())
        }
        StripeError::ApiError(msg) => ApiError::Internal(format!("Stripe API error: {msg}")),
        StripeError::Mongo(e) => ApiError::Internal(e.to_string()),
    }
}

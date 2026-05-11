use axum::{Json, extract::State};
use bson::oid::ObjectId;
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};

#[derive(Debug, Deserialize)]
pub struct CreateTenantRequest {
    pub name: String,
    pub slug: String,
}

#[derive(Debug, Serialize)]
pub struct TenantResponse {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub owner_id: String,
    pub plan: String,
}

pub async fn list(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Vec<TenantResponse>>, ApiError> {
    let tenants = state.tenants.find_user_tenants(auth.user_id).await?;

    let response: Vec<TenantResponse> = tenants
        .into_iter()
        .map(|t| TenantResponse {
            id: t.id.unwrap().to_hex(),
            name: t.name,
            slug: t.slug,
            owner_id: t.owner_id.to_hex(),
            // Plan enum is `#[serde(rename_all = "snake_case")]` so it
            // serializes as "free"/"pro"/"business"/"enterprise" via
            // serde. The frontend's plan cards (and Stripe /plans
            // response) use lowercase ids — Debug-formatting gives
            // "Free"/"Pro" which doesn't match, breaking the
            // currentPlan comparison.
            plan: format!("{:?}", t.plan).to_lowercase(),
        })
        .collect();

    Ok(Json(response))
}

pub async fn create(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<CreateTenantRequest>,
) -> Result<Json<TenantResponse>, ApiError> {
    let tenant = state
        .tenants
        .create(body.name, body.slug, auth.user_id)
        .await?;

    Ok(Json(TenantResponse {
        id: tenant.id.unwrap().to_hex(),
        name: tenant.name,
        slug: tenant.slug,
        owner_id: tenant.owner_id.to_hex(),
        plan: format!("{:?}", tenant.plan).to_lowercase(),
    }))
}

pub async fn get(
    State(state): State<AppState>,
    auth: AuthUser,
    axum::extract::Path(tenant_id): axum::extract::Path<String>,
) -> Result<Json<TenantResponse>, ApiError> {
    let tid = ObjectId::parse_str(&tenant_id)
        .map_err(|_| ApiError::BadRequest("Invalid tenant_id".to_string()))?;

    // Verify membership
    if !state.tenants.is_member(tid, auth.user_id).await? {
        return Err(ApiError::Forbidden("Not a member".to_string()));
    }

    let tenant = state.tenants.base.find_by_id(tid).await?;

    Ok(Json(TenantResponse {
        id: tenant.id.unwrap().to_hex(),
        name: tenant.name,
        slug: tenant.slug,
        owner_id: tenant.owner_id.to_hex(),
        plan: format!("{:?}", tenant.plan).to_lowercase(),
    }))
}

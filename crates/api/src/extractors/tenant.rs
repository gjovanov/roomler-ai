use axum::extract::{FromRequestParts, Path};
use axum::http::request::Parts;
use bson::oid::ObjectId;

use crate::error::ApiError;

/// Extracts tenant_id from the URL path parameter `:tenant_id`
#[derive(Debug, Clone)]
pub struct TenantId(pub ObjectId);

impl<S> FromRequestParts<S> for TenantId
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Path(params): Path<std::collections::HashMap<String, String>> =
            Path::from_request_parts(parts, state)
                .await
                .map_err(|_| ApiError::BadRequest("Missing path parameters".to_string()))?;

        let tid_str = params
            .get("tenant_id")
            .ok_or_else(|| ApiError::BadRequest("Missing tenant_id parameter".to_string()))?;

        let tenant_id = ObjectId::parse_str(tid_str)
            .map_err(|_| ApiError::BadRequest("Invalid tenant_id format".to_string()))?;

        Ok(TenantId(tenant_id))
    }
}

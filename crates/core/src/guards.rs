// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! Authorisation guards that need only the [`Core`] — usable from any
//! module's handlers.
//!
//! FR-69 P2 — moved from the api crate's `routes/stats.rs` unchanged, because
//! the first module (`saas`) gates its admin surface on the platform-admin
//! allowlist and a module cannot reach into the crate that composes it.

use bson::oid::ObjectId;
use roomler_ai_db::models::role::permissions;

use crate::{Core, error::ApiError, extractors::auth::AuthUser};

/// Platform-operator gate. 404 by design: the allowlist is
/// `ROOMLER__STATS__PLATFORM_ADMINS`, and a non-admin must not be able to tell
/// the surface exists.
pub fn require_platform_admin(state: &Core, auth: &AuthUser) -> Result<(), ApiError> {
    if state.platform_admins.contains(&auth.user_id) {
        Ok(())
    } else {
        Err(ApiError::NotFound("Not found".to_string()))
    }
}

/// Tenant-scope gate: membership (+ optionally MANAGE_AGENTS). Failures
/// are 404, not 403 — the web client wipes tokens on 403, and a member
/// removed from the org mid-poll must not be logged out of everything.
pub async fn require_tenant_stats(
    state: &Core,
    tenant_id: ObjectId,
    user_id: ObjectId,
    need_manage: bool,
) -> Result<(), ApiError> {
    let perms = state
        .tenants
        .get_member_permissions(tenant_id, user_id)
        .await
        .map_err(|_| ApiError::NotFound("Not found".to_string()))?;
    if need_manage && !permissions::has(perms, permissions::MANAGE_AGENTS) {
        return Err(ApiError::NotFound("Not found".to_string()));
    }
    Ok(())
}

pub fn parse_tid(tenant_id: &str) -> Result<ObjectId, ApiError> {
    ObjectId::parse_str(tenant_id).map_err(|_| ApiError::BadRequest("Invalid tenant_id".into()))
}

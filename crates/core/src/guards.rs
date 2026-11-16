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

/// Tenant-scope gate: membership (+ optionally MANAGE_AGENTS). Failures are
/// 404, not 403 — these are polled dashboards, and a caller who has just lost
/// the bit should see an empty panel rather than an error on every tick.
///
/// ⚠️ The reason recorded here used to be "the web client wipes tokens on
/// 403, and a member removed from the org mid-poll must not be logged out of
/// everything". It no longer does (FR-82): a 403 is an answer, not an expired
/// credential, and only the server-sent `not_a_member` code moves the client —
/// out of the tenant, never out of the session. Keep the 404; it is right for
/// the reason above, not for the one it was written for.
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

/// A rule or policy write names only roles of its own org: every id in
/// `requested` that `stored` does not already hold must be a role of
/// `tenant_id`, whoever writes it (ADMINISTRATOR included). 400 `bad_request`
/// `Unknown role id: <hex>` otherwise, the same answer for another org's role
/// and for no role at all.
///
/// ⚠️ An id the stored rule ALREADY names is accepted back as it is. The
/// editors send the stored lists back on every save, so a stored id of a role
/// deleted since must not make the rest of the rule unsaveable; it never
/// matches anyone, because every gate reads a member's roles live
/// (`TenantDao::member_live_role_ids`). ⚠️ Never DROP such an id either: an
/// empty role allow-list means "no restriction by role" on the device
/// policies, so removing the last id would widen a rule that matches nobody
/// into one that matches everybody.
pub async fn require_org_roles(
    state: &Core,
    tenant_id: ObjectId,
    requested: &[ObjectId],
    stored: &[ObjectId],
) -> Result<(), ApiError> {
    let new: Vec<ObjectId> = requested
        .iter()
        .copied()
        .filter(|r| !stored.contains(r))
        .collect();
    if new.is_empty() {
        return Ok(());
    }
    let org = state.tenants.org_role_ids_among(tenant_id, &new).await?;
    match new.iter().find(|r| !org.contains(r)) {
        Some(unknown) => Err(ApiError::BadRequest(format!(
            "Unknown role id: {}",
            unknown.to_hex()
        ))),
        None => Ok(()),
    }
}

/// Tenant-permission gate: the caller must hold `flag` in `tenant_id`.
///
/// FR-69 P3 — moved from the api crate's `routes/remote_control.rs`, where
/// every device-management surface gated its destructive endpoints on it; the
/// `chat` module's room routes gate on `MANAGE_CHANNELS` through the same
/// function. 403 with the permission's name, unlike the platform-admin gate
/// above: membership is not a secret, the missing bit is useful to know.
pub async fn require_permission(
    state: &Core,
    tenant_id: ObjectId,
    user_id: ObjectId,
    flag: u64,
    label: &str,
) -> Result<(), ApiError> {
    let perms = state
        .tenants
        .get_member_permissions(tenant_id, user_id)
        .await?;
    if !permissions::has(perms, flag) {
        return Err(ApiError::Forbidden(format!("Missing {label} permission")));
    }
    Ok(())
}

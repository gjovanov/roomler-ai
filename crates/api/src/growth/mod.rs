// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-102 — growth observability, the host's side (spec §3o).
//!
//! Everything under `/api/admin/growth/*` is a view over core (users,
//! tenants, the exclusion list) and whichever modules are mounted (fleet's
//! agents, remote's sessions, conference's calls, saas's subscribers) — a
//! composition view, which is the host's to hold, next to `/admin/stats`.
//! Same gate as its siblings: 404 to anyone who is not a platform admin,
//! and 404 for everyone when `growth.enabled = false` (the kill switch).
//!
//! * [`exclusions`] — P1c: the explicit exclusion list and the implicit one
//!   it folds into (`roomler_core::growth::ExclusionSet`).

pub mod exclusions;

use axum::{Router, routing::get};

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};

/// The `/api/admin/growth` router, mounted by `build_router`.
pub fn admin_routes() -> Router<AppState> {
    Router::new().route(
        "/exclusions",
        get(exclusions::get_exclusions).put(exclusions::put_exclusions),
    )
}

/// The gate every growth handler passes first: a platform admin, on a
/// server whose growth surface is on. Both failures are 404 — the surface
/// is what is being hidden.
pub(crate) fn require_growth_admin(state: &AppState, auth: &AuthUser) -> Result<(), ApiError> {
    crate::routes::stats::require_platform_admin(state, auth)?;
    if !state.settings.growth.enabled {
        return Err(ApiError::NotFound("Not found".to_string()));
    }
    Ok(())
}

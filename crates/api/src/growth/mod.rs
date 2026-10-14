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
//! * [`digest`] — P1d: the daily digest's sections, the once-only claim,
//!   the delivery and the scheduler; [`purestat`] its one read of the web
//!   plane; [`render`] its email bodies; [`routes`] its four routes.

pub mod digest;
pub mod exclusions;
pub mod purestat;
pub mod render;
pub mod routes;

use axum::{
    Router,
    routing::{get, post},
};

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};

/// The `/api/admin/growth` router, mounted by `build_router`.
pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/exclusions",
            get(exclusions::get_exclusions).put(exclusions::put_exclusions),
        )
        // P1d — the digest: a stored day, the list, a recompute (mails only
        // with `send=true` and never a `sent` day), and a live preview that
        // stores and mails nothing (`include_internal=true` = raw totals).
        .route("/digest", get(routes::get_digest))
        .route("/digests", get(routes::list_digests))
        .route("/digest/run", post(routes::run_digest))
        .route("/digest/preview", get(routes::preview_digest))
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

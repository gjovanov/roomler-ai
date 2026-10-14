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
//!   plane (in `roomler-ai-services` since P6, re-exported here); [`render`]
//!   its email bodies; [`routes`] its four routes.
//! * [`value`] — P2: the four value-event definitions every view shares.
//! * [`org_daily`] — P2: `growth_org_daily`, the nightly fold and the
//!   bounded backfill.
//! * [`funnel`], [`cohorts`], [`orgs`] — P2: the account funnel with time
//!   to value, the weekly cohorts with the north star (WAO), the org list
//!   and one org's timeline.
//! * [`campaigns`] — P6: the Campaigns table, purestat's web plane queried
//!   live for the range beside the funnel's account plane, joined by the
//!   normalised campaign key. (The daily purestat PULL is the `saas`
//!   module's — it talks to the outside world for the hosted business.)
//! * [`channels`], [`keys`] — P8: the Channels table (each platform's own
//!   reach as the promotion pipeline pushed it, then what each post's key
//!   brought, and spend) and the machine keys that open the `saas` module's
//!   ingest routes.

pub mod campaigns;
pub mod channels;
pub mod cohorts;
pub mod digest;
pub mod exclusions;
pub mod funnel;
pub mod keys;
pub mod org_daily;
pub mod orgs;
pub mod render;
pub mod routes;
pub mod value;

/// The purestat v1 client lives in `roomler-ai-services` since P6 — below
/// the host AND the `saas` module, whose daily puller uses it too.
/// Re-exported so `growth::purestat::…` keeps naming it.
pub use roomler_ai_services::purestat;

use axum::{
    Router,
    routing::{delete, get, post},
};

use crate::{error::ApiError, extractors::auth::AuthUser, state::AppState};

/// The one error type of the growth surface: the digest's, which every
/// later phase reuses rather than minting a twin.
pub use digest::DigestError as GrowthError;

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
        // P2 — the account funnel (with time to value), the weekly cohorts
        // (with WAO), the org list, one org's timeline, and the bounded
        // backfill of `growth_org_daily`.
        .route("/funnel", get(funnel::get_funnel))
        .route("/cohorts", get(cohorts::get_cohorts))
        .route("/orgs", get(orgs::list_orgs))
        .route("/org/{tenant_id}/timeline", get(orgs::org_timeline))
        .route("/org-daily/backfill", post(org_daily::backfill_route))
        // P6 — the Campaigns table: per normalised key, purestat's web
        // plane (live, for the exact range) beside the account funnel.
        .route("/campaigns", get(campaigns::get_campaigns))
        // P8 — the Channels table (each platform's own reach, then what the
        // posts' keys brought, and spend) and the growth machine keys (D12:
        // only a hash stored; an `ingest` key opens the `saas` module's
        // `/admin/growth/ingest/*` routes and nothing else).
        .route("/channels", get(channels::get_channels))
        .route("/keys", get(keys::list_keys).post(keys::mint_key))
        .route("/keys/{key_id}", delete(keys::revoke_key))
}

/// Whether a module is mounted on this pod — the gate every growth read of
/// a module's collection passes, so an absent module reads as "cannot
/// see", never as "none".
pub(crate) fn mounted(state: &AppState, id: &str) -> bool {
    state.modules.mounted().contains(&id)
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

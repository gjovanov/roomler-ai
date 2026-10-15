// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-102 P4 — plan-limit checks that also COUNT the collision, per org.
//!
//! The decision stays `roomler_ai_services::quota`'s (FR-32: one decision
//! point, the warn line written there exactly once). What this adds is the
//! business model's "best upgrade-intent signal" (`docs/business-model.md`
//! §5) as a number: every collision — refused or, in `warn` mode, let
//! through — bumps `growth_daily {source: quota, metric: collision, dims:
//! {tenant, limit}}` for the UTC day, beside the warn line. Those are the
//! orgs that keep hitting the ceiling.
//!
//! The call sites pass the TENANT (which the pure decision never needed),
//! through `&Core` so the count lands in that server's own database — a
//! process-wide sink would cross test servers in one process, and a
//! self-hosted server keeps its own counts and sends them nowhere.
//!
//! The write is spawned and best-effort (a refused request is answered at
//! once whatever the database is doing), and off with `growth.enabled`.

use std::collections::BTreeMap;

use roomler_ai_db::models::Tenant;
use roomler_ai_services::{
    dao::growth::{GrowthDao, QUOTA_COLLISION_METRIC, QUOTA_SOURCE},
    quota::{self, Limit, QuotaDenial},
};
use tracing::warn;

use crate::Core;

/// [`quota::check`] for `tenant`, counting the collision.
pub fn check(core: &Core, tenant: &Tenant, limit: Limit, used: u64) -> Result<(), QuotaDenial> {
    check_delta(core, tenant, limit, used, 1)
}

/// [`quota::check_delta`] for `tenant`, counting the collision.
pub fn check_delta(
    core: &Core,
    tenant: &Tenant,
    limit: Limit,
    current: u64,
    delta: u64,
) -> Result<(), QuotaDenial> {
    let verdict = quota::evaluate(
        tenant.plan.clone(),
        tenant.settings.plan_enforcement,
        limit,
        current,
        delta,
    );
    if verdict.collision.is_some() {
        record_collision(core, tenant, limit);
    }
    verdict.into_result()
}

/// [`quota::require_feature`] for `tenant`, counting the collision.
pub fn require_feature(core: &Core, tenant: &Tenant, limit: Limit) -> Result<(), QuotaDenial> {
    check(core, tenant, limit, 0)
}

/// The `growth_daily` dimensions of one collision: the org (its hex id) and
/// the limit (its wire spelling). Never a person.
pub fn collision_dims(tenant_hex: &str, limit: Limit) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("tenant".to_string(), tenant_hex.to_string()),
        ("limit".to_string(), limit.as_str().to_string()),
    ])
}

fn record_collision(core: &Core, tenant: &Tenant, limit: Limit) {
    if !core.settings.growth.enabled {
        return;
    }
    let Some(tenant_id) = tenant.id else {
        return;
    };
    let db = core.db.clone();
    // `YYYY-MM-DD`, UTC — the day the collision happened.
    let day: String = bson::DateTime::now()
        .try_to_rfc3339_string()
        .unwrap_or_default()
        .chars()
        .take(10)
        .collect();
    tokio::spawn(async move {
        let dims = collision_dims(&tenant_id.to_hex(), limit);
        if let Err(e) = GrowthDao::new(&db)
            .bump_daily(&day, QUOTA_SOURCE, QUOTA_COLLISION_METRIC, &dims, 1)
            .await
        {
            warn!(%e, limit = limit.as_str(), "quota collision could not be counted");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_collision_is_the_org_and_the_limit_never_a_person() {
        let dims = collision_dims("65f000000000000000000001", Limit::MaxMembers);
        assert_eq!(dims.len(), 2);
        assert_eq!(dims["tenant"], "65f000000000000000000001");
        assert_eq!(dims["limit"], "max_members");
    }
}

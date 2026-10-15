// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-102 P1c — the exclusion set every growth number applies.
//!
//! Internal orgs and people must not count (spec §3c item 4): the platform
//! admins' own sign-ups, the orgs they create to test with, the throwaway
//! orgs a field test leaves behind. Two kinds of exclusion fold into ONE set:
//!
//! * **implicit** — every platform admin (`ROOMLER__STATS__PLATFORM_ADMINS`)
//!   and every org owned by an excluded user. No row needed: the allowlist
//!   is the record, and an admin who creates a new test org tomorrow is
//!   excluded with it.
//! * **explicit** — the `growth_exclusions` rows a platform admin writes
//!   (`PUT /api/admin/growth/exclusions`), each with a reason.
//!
//! [`ExclusionSet::load`] is the one function; a growth number that reads
//! the collection directly, or applies only the explicit rows, is wrong by
//! construction. `include_internal=true` is [`ExclusionSet::none`] — the raw
//! totals, with the set empty rather than the filter skipped, so the code
//! path is the same either way.

use std::collections::HashSet;

use bson::{Document, doc, oid::ObjectId};
use futures::TryStreamExt;
use roomler_ai_services::dao::{
    base::DaoResult,
    growth::{ExclusionKind, GrowthDao, GrowthExclusion},
};

use crate::Core;

/// Why an implicit exclusion applies — the label the admin view shows.
pub const IMPLICIT_PLATFORM_ADMIN: &str = "platform admin";
pub const IMPLICIT_OWNED_BY_EXCLUDED: &str = "owned by an excluded user";

#[derive(Debug, Clone, Default)]
pub struct ExclusionSet {
    /// Every excluded user id, implicit and explicit.
    pub users: HashSet<ObjectId>,
    /// Every excluded tenant id, implicit and explicit.
    pub tenants: HashSet<ObjectId>,
    /// The explicit rows, as stored.
    pub explicit: Vec<GrowthExclusion>,
    /// The platform admins (implicit users).
    pub implicit_users: Vec<ObjectId>,
    /// The orgs owned by an excluded user (implicit tenants), minus any that
    /// also has an explicit row.
    pub implicit_tenants: Vec<ObjectId>,
}

impl ExclusionSet {
    /// The empty set: `include_internal=true`.
    pub fn none() -> Self {
        Self::default()
    }

    /// Platform admins + the explicit rows + every org owned by any of those
    /// users. One read of `growth_exclusions`, one of `tenants`.
    pub async fn load(core: &Core) -> DaoResult<Self> {
        let explicit = GrowthDao::new(&core.db).list_exclusions().await?;
        let mut implicit_users: Vec<ObjectId> = core.platform_admins.iter().copied().collect();
        implicit_users.sort_unstable();

        let mut users: HashSet<ObjectId> = implicit_users.iter().copied().collect();
        let mut tenants: HashSet<ObjectId> = HashSet::new();
        for row in &explicit {
            match row.kind {
                ExclusionKind::User => {
                    users.insert(row.id);
                }
                ExclusionKind::Tenant => {
                    tenants.insert(row.id);
                }
            }
        }

        // Tombstoned orgs included: an archived test org is still internal.
        let owners: Vec<ObjectId> = users.iter().copied().collect();
        let mut implicit_tenants = Vec::new();
        if !owners.is_empty() {
            let mut cursor = core
                .db
                .collection::<Document>("tenants")
                .find(doc! { "owner_id": { "$in": &owners } })
                .projection(doc! { "_id": 1 })
                .await?;
            while let Some(d) = cursor.try_next().await? {
                if let Ok(id) = d.get_object_id("_id")
                    && tenants.insert(id)
                {
                    implicit_tenants.push(id);
                }
            }
        }
        implicit_tenants.sort_unstable();

        Ok(Self {
            users,
            tenants,
            explicit,
            implicit_users,
            implicit_tenants,
        })
    }

    pub fn user_excluded(&self, id: &ObjectId) -> bool {
        self.users.contains(id)
    }

    pub fn tenant_excluded(&self, id: &ObjectId) -> bool {
        self.tenants.contains(id)
    }

    /// `(users, tenants)` — what the digest footer reports as left out.
    pub fn counts(&self) -> (usize, usize) {
        (self.users.len(), self.tenants.len())
    }

    /// The `$nin` filter for a user-keyed query (`users._id`).
    pub fn user_filter(&self, field: &str) -> Document {
        let ids: Vec<ObjectId> = self.users.iter().copied().collect();
        doc! { field: { "$nin": ids } }
    }

    /// The `$nin` filter for a tenant-keyed query.
    pub fn tenant_filter(&self, field: &str) -> Document {
        let ids: Vec<ObjectId> = self.tenants.iter().copied().collect();
        doc! { field: { "$nin": ids } }
    }
}

/// P2 — everything the growth collections hold about one org, removed:
/// its `growth_org_daily` rows and the `growth_daily` rows whose dims carry
/// it (spec principle 7: aggregates may outlive rows; rows never outlive
/// their owner). This is the ONE call an org deletion must make for growth.
///
/// ⚠️ There is no org hard-delete path today — orgs are only archived, and
/// reversibly (`crates/api/src/routes/tenant.rs`), so nothing in production
/// calls this yet and the 25-month TTL is what bounds the collection. It is
/// deliberately NOT wired into the archive cascade: an archived org keeps
/// everything it knows so that `unarchive` restores a working org, and its
/// activity history is part of that. When a real deletion path lands, it
/// calls this.
///
/// FR-102 P4 — the org's `billing_events` go too (§3o: "with the org"); the
/// monthly billed totals frozen into `growth_daily` carry no tenant and stay,
/// so deleting an org never rewrites last year's MRR.
pub async fn purge_tenant(
    core: &Core,
    tenant_id: ObjectId,
) -> DaoResult<roomler_ai_services::dao::growth::PurgedCounts> {
    let mut counts = GrowthDao::new(&core.db).purge_tenant(tenant_id).await?;
    counts.billing_events = roomler_ai_services::dao::billing_event::BillingEventDao::new(&core.db)
        .purge_tenant(tenant_id)
        .await?;
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_empty_set_excludes_nothing() {
        let none = ExclusionSet::none();
        assert!(!none.user_excluded(&ObjectId::new()));
        assert!(!none.tenant_excluded(&ObjectId::new()));
        assert_eq!(none.counts(), (0, 0));
        let f = none.user_filter("_id");
        assert_eq!(
            f.get_document("_id")
                .unwrap()
                .get_array("$nin")
                .unwrap()
                .len(),
            0
        );
    }
}

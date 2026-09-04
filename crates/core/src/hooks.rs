// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! The inverse edges: what core tells modules, and in what order.
//!
//! Calls flow module → core along the DAG in [`crate::graph`]. The flows that
//! go the other way today — a tenant archive releasing every overlay node, an
//! agent's removal terminating its sessions and releasing its lease, presence
//! changes — become hooks a module registers at init and core invokes
//! **synchronously, in-process, in a fixed order**. Not an event bus: eventual
//! consistency on a cascade that must not pool an address before its tombstone
//! is the bug class the overlay IPAM already paid for (FR-69 D6).
//!
//! Every hook must be idempotent — a cascade can be re-run after a crash
//! mid-way, which is already true of today's code.

use std::sync::Arc;

use async_trait::async_trait;
use bson::oid::ObjectId;

/// The order core invokes hooks in, for every event: the modules that hold
/// sessions first, then the ones that hold leases, then the owner of the
/// record being removed. Written once, here, so it is a fact and not a
/// convention spread over call sites.
pub const HOOK_ORDER: &[&str] = &["remote", "network", "fleet", "conference", "chat", "saas"];

/// Tenant lifecycle events, owned by core.
#[async_trait]
pub trait TenantLifecycle: Send + Sync {
    /// The tenant was archived: release what the module holds for it. The
    /// `reason` is the audit string core recorded.
    async fn tenant_archived(&self, tenant_id: ObjectId, reason: &str) -> anyhow::Result<()> {
        let _ = (tenant_id, reason);
        Ok(())
    }

    /// A member left or was removed: drop what the module holds for that user
    /// in that tenant (sessions, subscriptions, sockets).
    async fn member_removed(&self, tenant_id: ObjectId, user_id: ObjectId) -> anyhow::Result<()> {
        let _ = (tenant_id, user_id);
        Ok(())
    }
}

/// Device lifecycle events, owned by `fleet` and relayed through core so
/// `remote` and `network` never have to be called by `fleet` directly.
#[async_trait]
pub trait FleetLifecycle: Send + Sync {
    /// The agent row is being removed (delete cascade, admin evict, ephemeral
    /// reap): terminate sessions, release leases. Runs BEFORE the row is
    /// tombstoned — the order that only ever leaks, never double-allocates.
    async fn agent_removed(
        &self,
        tenant_id: ObjectId,
        agent_id: ObjectId,
        reason: &str,
    ) -> anyhow::Result<()> {
        let _ = (tenant_id, agent_id, reason);
        Ok(())
    }

    /// The agent's control socket went away.
    async fn agent_offline(&self, tenant_id: ObjectId, agent_id: ObjectId) -> anyhow::Result<()> {
        let _ = (tenant_id, agent_id);
        Ok(())
    }
}

/// What a module registers. Both are optional; a module with nothing to
/// release registers nothing and costs nothing.
#[derive(Clone, Default)]
pub struct Hooks {
    pub tenant: Option<Arc<dyn TenantLifecycle>>,
    pub fleet: Option<Arc<dyn FleetLifecycle>>,
}

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hooks")
            .field("tenant", &self.tenant.is_some())
            .field("fleet", &self.fleet.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::MODULES;

    #[test]
    fn hook_order_covers_every_module_exactly_once() {
        let mut sorted: Vec<&str> = HOOK_ORDER.to_vec();
        sorted.sort_unstable();
        let mut modules: Vec<&str> = MODULES.to_vec();
        modules.sort_unstable();
        assert_eq!(sorted, modules);
    }

    #[test]
    fn session_holders_run_before_lease_holders_before_the_record_owner() {
        let pos = |id: &str| HOOK_ORDER.iter().position(|m| *m == id).unwrap();
        assert!(pos("remote") < pos("network"));
        assert!(pos("network") < pos("fleet"));
    }
}
